use std::{
    collections::VecDeque,
    io::{Read, Write},
    os::fd::{AsRawFd, OwnedFd, RawFd},
    sync::{Arc, Mutex, mpsc as std_mpsc},
    time::{Duration, Instant},
};

use bytes::Bytes;
use shepr_core::layout::PaneId;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, error, warn};

pub use crate::submission::{QueuedSubmission, SubmissionCancel, SubmissionCancelOutcome};
use crate::{
    fd,
    submission::{EnterStart, SharedSubmissionState, SubmissionPart, SubmissionState, lock_state},
};

// Actor handle methods must call wake_actor() after queuing work. The idle
// timeout is only a fallback for missed wakes; PTY and wake readiness drive
// normal responsiveness.
const ACTOR_IDLE_POLL_MS: i32 = 1000;
/// Unwritten bytes the inbox holds across user input, submissions and
/// terminal replies. A single item larger than this is still admitted when
/// nothing else is outstanding (see `PtyIoInbox::reserve`).
const ACTOR_INBOX_MAX_BYTES: usize = 256 * 1024;
const ACTOR_INBOX_MAX_ITEMS: usize = 1024;
const RESIZE_RETRY_BASE: Duration = Duration::from_millis(50);
const RESIZE_RETRY_MAX: Duration = Duration::from_secs(5);
/// Failed ioctl attempts (50 + 100 + 200 + 400 + 800 ms of backoff) a resize
/// may hold its replies, and every write queued after them, before the
/// replies are released without it. The ioctl itself keeps retrying.
const RESIZE_HOLD_ATTEMPTS: u8 = 5;
/// Write steps one pump may take before it returns to poll, so a steady
/// stream of input cannot starve reads of the child's output.
const MAX_WRITE_STEPS_PER_PUMP: usize = 64;

pub struct PtyReadResult {
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
    fn empty() -> Self {
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
pub enum ReaderExit {
    /// EOF, an IO error or a shutdown request. The
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
    geometry: shepr_core::geometry::PaneGeometry,
}

pub struct PtyIoActorConfig {
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

fn submission_withdrawn_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "input submission withdrawn by its caller",
    )
}

#[derive(Clone)]
pub struct PtyIoActorHandle {
    wake: fd::WakeWriter,
    inbox: Arc<Mutex<PtyIoInbox>>,
    /// Held across producing a terminal reply and queuing it, by
    /// `write_terminal_response` and by the actor around parsing a read, so
    /// replies enter the inbox in the order the terminal produced them. It is
    /// separate from the inbox lock so that user input is never queued behind
    /// a parse or a wait for the terminal core.
    response_order: Arc<Mutex<()>>,
}

/// Everything waiting to reach the PTY, shared by the handles and the actor.
/// Producers only push to the back of `entries` (or replace
/// `latest_resize`); only the actor removes or inserts, so an index it takes
/// stays valid across a moment with the lock released. The lock is never
/// held across a syscall.
#[derive(Default)]
struct PtyIoInbox {
    entries: VecDeque<PtyIoInboxEntry>,
    pending_bytes: usize,
    pending_items: usize,
    next_order: u64,
    latest_resize: Option<QueuedResize>,
    shutdown: bool,
}

struct PtyIoInboxEntry {
    order: u64,
    kind: PtyIoInboxEntryKind,
}

enum PtyIoInboxEntryKind {
    Write(PendingWrite),
    Submission {
        text: Bytes,
        enter: Bytes,
        delay: Duration,
        reply: std_mpsc::Sender<std::io::Result<()>>,
        state: SharedSubmissionState,
    },
}

struct QueuedResize {
    order: u64,
    resize: PtyResize,
    terminal_responses: Vec<Bytes>,
    retry_at: Option<Instant>,
    attempts: u8,
}

impl PtyIoInbox {
    fn next_order(&mut self) -> u64 {
        let order = self.next_order;
        self.next_order = self.next_order.wrapping_add(1);
        order
    }

    /// Admit work while the outstanding total stays within the caps. One item
    /// larger than the byte cap is admitted when no bytes are outstanding, so
    /// a big paste or prompt still reaches a pane that is reading instead of
    /// being refused forever; the bound is then that single item, which the
    /// caller already held in memory.
    fn reserve(&mut self, bytes: usize, items: usize) -> bool {
        let Some(pending_items) = self.pending_items.checked_add(items) else {
            return false;
        };
        if pending_items > ACTOR_INBOX_MAX_ITEMS {
            return false;
        }
        let pending_bytes = match self.pending_bytes.checked_add(bytes) {
            Some(total) if total <= ACTOR_INBOX_MAX_BYTES || self.pending_bytes == 0 => total,
            _ => return false,
        };
        self.pending_bytes = pending_bytes;
        self.pending_items = pending_items;
        true
    }

    fn release_bytes(&mut self, bytes: usize) {
        self.pending_bytes = self.pending_bytes.saturating_sub(bytes);
    }

    fn release_item(&mut self) {
        self.pending_items = self.pending_items.saturating_sub(1);
    }

    fn push_user_input(&mut self, bytes: Bytes) -> Result<(), Bytes> {
        if bytes.is_empty() {
            return Ok(());
        }
        if !self.reserve(bytes.len(), 1) {
            return Err(bytes);
        }
        let order = self.next_order();
        self.entries.push_back(PtyIoInboxEntry {
            order,
            kind: PtyIoInboxEntryKind::Write(PendingWrite::User(bytes)),
        });
        Ok(())
    }

    fn push_terminal_response(&mut self, bytes: Bytes) -> bool {
        if bytes.is_empty() {
            return true;
        }
        // Only a child that has stopped reading fills the inbox, and a reply
        // it will read late is worth little, so an overflowing reply is
        // dropped; user input instead gets Full. The replies already queued
        // keep their order, and a later reply that fits still goes out (a
        // DA1 sentinel behind a dropped answer tells the child the answer is
        // not coming rather than leaving it waiting).
        if !self.reserve(bytes.len(), 1) {
            return false;
        }
        let order = self.next_order();
        self.entries.push_back(PtyIoInboxEntry {
            order,
            kind: PtyIoInboxEntryKind::Write(PendingWrite::TerminalResponse(bytes)),
        });
        true
    }

    fn push_submission(
        &mut self,
        text: Bytes,
        enter: Bytes,
        delay: Duration,
        reply: std_mpsc::Sender<std::io::Result<()>>,
        state: SharedSubmissionState,
    ) -> Result<(), ()> {
        let Some(bytes) = text.len().checked_add(enter.len()) else {
            return Err(());
        };
        if !self.reserve(bytes, 1) {
            return Err(());
        }
        let order = self.next_order();
        self.entries.push_back(PtyIoInboxEntry {
            order,
            kind: PtyIoInboxEntryKind::Submission {
                text,
                enter,
                delay,
                reply,
                state,
            },
        });
        Ok(())
    }

    /// Coalesce resizes: only the newest geometry matters to the PTY, and the
    /// replies of a superseded request describe a size that no longer holds.
    fn replace_resize(
        &mut self,
        geometry: shepr_core::geometry::PaneGeometry,
        terminal_responses: Vec<Bytes>,
    ) {
        if let Some(previous) = self.latest_resize.take() {
            for bytes in previous.terminal_responses {
                self.release_bytes(bytes.len());
                self.release_item();
            }
        }

        let mut accepted_responses = Vec::new();
        for bytes in terminal_responses {
            if bytes.is_empty() {
                continue;
            }
            if self.reserve(bytes.len(), 1) {
                accepted_responses.push(bytes);
            }
        }
        let order = self.next_order();
        self.latest_resize = Some(QueuedResize {
            order,
            resize: PtyResize { geometry },
            terminal_responses: accepted_responses,
            retry_at: None,
            attempts: 0,
        });
    }

    /// The entry the actor should handle next. A write already under way
    /// always continues. While a submission is active, user input queued
    /// behind it waits, but terminal replies and the submission's own parts
    /// go out. Otherwise it is the front of the queue.
    fn next_entry_index(
        &self,
        active_submission: bool,
        current_order: Option<u64>,
    ) -> Option<usize> {
        if let Some(order) = current_order {
            return self.entries.iter().position(|entry| entry.order == order);
        }
        if active_submission {
            self.entries.iter().position(|entry| {
                matches!(
                    &entry.kind,
                    PtyIoInboxEntryKind::Write(
                        PendingWrite::TerminalResponse(_) | PendingWrite::Submission { .. }
                    )
                )
            })
        } else {
            (!self.entries.is_empty()).then_some(0)
        }
    }

    /// Whether a failed resize still holds its replies ahead of an entry.
    /// Everything queued after those replies waits with them, so no later
    /// reply overtakes them.
    fn resize_holds(&self, order: u64) -> bool {
        self.latest_resize
            .as_ref()
            .is_some_and(|resize| !resize.terminal_responses.is_empty() && resize.order < order)
    }

    /// The write the actor can start or continue now, if any.
    fn writable_index(&self, active_submission: bool, current_order: Option<u64>) -> Option<usize> {
        let index = self.next_entry_index(active_submission, current_order)?;
        let entry = self.entries.get(index)?;
        if !matches!(entry.kind, PtyIoInboxEntryKind::Write(_)) {
            return None;
        }
        // A write under way was started before any resize that could hold it.
        if current_order.is_none() && self.resize_holds(entry.order) {
            return None;
        }
        Some(index)
    }

    /// Remove an entry and release what it reserved. `written` bytes of it
    /// were already released as they were written. A submission part holds
    /// no item of its own: the submission's item is released when the
    /// submission finishes.
    fn remove_entry(&mut self, index: usize, written: usize) -> Option<PtyIoInboxEntryKind> {
        let entry = self.entries.remove(index)?;
        let (bytes, releases_item) = match &entry.kind {
            PtyIoInboxEntryKind::Write(
                PendingWrite::User(bytes) | PendingWrite::TerminalResponse(bytes),
            ) => (bytes.len(), true),
            PtyIoInboxEntryKind::Write(PendingWrite::Submission { bytes, .. }) => {
                (bytes.len(), false)
            }
            PtyIoInboxEntryKind::Submission { text, enter, .. } => {
                (text.len().saturating_add(enter.len()), true)
            }
        };
        self.release_bytes(bytes.saturating_sub(written));
        if releases_item {
            self.release_item();
        }
        Some(entry.kind)
    }

    /// Queue a resize's replies at the resize's place in the sequence: after
    /// everything produced before the resize, ahead of everything after it.
    /// Their bytes and items were reserved when the resize was queued.
    fn insert_resize_replies(&mut self, order: u64, replies: Vec<Bytes>) {
        let insert_at = self
            .entries
            .iter()
            .position(|entry| entry.order > order)
            .unwrap_or(self.entries.len());
        for (offset, bytes) in replies.into_iter().enumerate() {
            self.entries.insert(
                insert_at + offset,
                PtyIoInboxEntry {
                    order,
                    kind: PtyIoInboxEntryKind::Write(PendingWrite::TerminalResponse(bytes)),
                },
            );
        }
    }
}

impl PtyIoActorHandle {
    pub fn try_write_user_input(&self, bytes: Bytes) -> Result<(), TrySendError<Bytes>> {
        let result = {
            let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
            if inbox.shutdown {
                return Err(TrySendError::Closed(bytes));
            }
            inbox.push_user_input(bytes)
        };
        match result {
            Ok(()) => {
                self.wake_actor();
                Ok(())
            }
            Err(bytes) => Err(TrySendError::Full(bytes)),
        }
    }

    pub fn queue_user_input_submission(
        &self,
        text: Bytes,
        enter: Bytes,
        delay: Duration,
    ) -> std::io::Result<QueuedSubmission> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        let state = SubmissionState::shared();
        {
            let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
            if inbox.shutdown {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "pty actor closed",
                ));
            }
            inbox
                .push_submission(text, enter, delay, reply_tx, Arc::clone(&state))
                .map_err(|()| {
                    std::io::Error::new(std::io::ErrorKind::WouldBlock, "pty input queue is full")
                })?;
        }
        self.wake_actor();
        Ok(QueuedSubmission {
            completion: reply_rx,
            cancel: SubmissionCancel {
                state,
                wake: Some(self.wake.clone()),
            },
        })
    }

    /// Queue a terminal reply produced outside a PTY read. `response` runs
    /// under the reply-order lock (it may take the terminal core lock), never
    /// under the inbox lock.
    pub fn write_terminal_response(&self, response: impl FnOnce() -> Option<Bytes>) {
        let _order = crate::locks::lock_auxiliary(&self.response_order);
        let Some(bytes) = response() else {
            return;
        };
        let queued = {
            let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
            !inbox.shutdown && inbox.push_terminal_response(bytes)
        };
        if queued {
            self.wake_actor();
        }
    }

    /// Produce resize replies and queue them at one point in the response
    /// order. The closure may take the terminal core lock, so it runs under
    /// `response_order` but never under the inbox lock.
    pub fn resize(
        &self,
        geometry: shepr_core::geometry::PaneGeometry,
        terminal_responses: impl FnOnce() -> Vec<Bytes>,
    ) {
        let _order = crate::locks::lock_auxiliary(&self.response_order);
        let terminal_responses = terminal_responses();
        let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
        if inbox.shutdown {
            return;
        }
        inbox.replace_resize(geometry, terminal_responses);
        drop(inbox);
        drop(_order);
        self.wake_actor();
    }

    pub fn shutdown(&self) {
        let changed = {
            let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
            if inbox.shutdown {
                false
            } else {
                inbox.shutdown = true;
                true
            }
        };
        if changed {
            self.wake_actor();
        }
    }

    fn wake_actor(&self) {
        if let Err(err) = self.wake.wake() {
            debug!(err = %err, "failed to wake PTY actor");
        }
    }
}

pub struct PtyIoActor;

impl PtyIoActor {
    pub fn spawn(config: PtyIoActorConfig) -> std::io::Result<PtyIoActorHandle> {
        Self::spawn_inner(config, None)
    }

    fn spawn_inner(
        config: PtyIoActorConfig,
        poll_observer: Option<std_mpsc::Sender<()>>,
    ) -> std::io::Result<PtyIoActorHandle> {
        fd::set_cloexec(config.master_fd.as_raw_fd())?;
        fd::set_nonblocking(config.master_fd.as_raw_fd())?;

        let wake_pipe = fd::create_wake_pipe()?;
        let inbox = Arc::new(Mutex::new(PtyIoInbox::default()));
        let response_order = Arc::new(Mutex::new(()));
        let handle = PtyIoActorHandle {
            wake: wake_pipe.writer,
            inbox: Arc::clone(&inbox),
            response_order: Arc::clone(&response_order),
        };

        let mut runner = PtyIoActorRunner {
            pane_id: config.pane_id,
            file: std::fs::File::from(config.master_fd),
            inbox,
            response_order,
            current_write_order: None,
            current_write_offset: 0,
            active_submission: None,
            wake_read_fd: wake_pipe.read_fd,
            on_read: config.on_read,
            on_reader_exit: config.on_reader_exit,
            core_broken: config.core_broken,
            read_callback_panicked: false,
            poll_observer,
            resize_pty: Box::new(resize_pty),
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
    inbox: Arc<Mutex<PtyIoInbox>>,
    response_order: Arc<Mutex<()>>,
    /// The entry whose write is under way, and how much of it is written.
    /// The offset is zero whenever the order is `None`.
    current_write_order: Option<u64>,
    current_write_offset: usize,
    active_submission: Option<ActiveSubmission>,
    wake_read_fd: OwnedFd,
    on_read: ReadCallback,
    on_reader_exit: Option<ReaderExitCallback>,
    core_broken: Option<CoreBrokenCheck>,
    read_callback_panicked: bool,
    poll_observer: Option<std_mpsc::Sender<()>>,
    resize_pty: Box<dyn FnMut(RawFd, PtyResize) -> std::io::Result<()> + Send>,
}

struct ActiveSubmission {
    enter: Bytes,
    /// Enter bytes reserved in the inbox but not yet queued as an entry.
    unqueued_enter_bytes: usize,
    reply: std_mpsc::Sender<std::io::Result<()>>,
    state: SharedSubmissionState,
}

#[derive(Debug, PartialEq, Eq)]
enum WriteStep {
    /// Nothing can be written now.
    Idle,
    /// The PTY would block.
    Blocked,
    /// Bytes were written or an entry was retired; a finished submission
    /// part is returned for `complete_submission_part`.
    Progress(Option<SubmissionPart>),
}

#[derive(Debug, PartialEq, Eq)]
enum PendingWrite {
    User(Bytes),
    TerminalResponse(Bytes),
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
    fn run(&mut self) {
        loop {
            if crate::locks::lock_auxiliary(&self.inbox).shutdown {
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

            // The wake pipe is drained right after poll returns, before this
            // pump reads the inbox, so work pushed after the pump has read it
            // always leaves a wake byte for the poll below.
            if let Err(err) = self.pump() {
                self.handle_write_failure(err);
                break;
            }
            if let Some(poll_observer) = &self.poll_observer {
                // A test hook; a test that stopped listening wants no more
                // poll notices, so a closed receiver is not an error.
                poll_observer.send(()).ok();
            }

            match fd::poll_pty_and_wake(
                self.file.as_raw_fd(),
                self.wake_read_fd.as_raw_fd(),
                self.has_writable_work(),
                self.poll_timeout_ms(),
            ) {
                Ok(readiness) => {
                    if readiness.wake_ready
                        && let Err(err) = fd::drain_wake_fd(self.wake_read_fd.as_raw_fd())
                    {
                        debug!(pane = self.pane_id.raw(), err = %err, "PTY actor wake drain failed");
                        break;
                    }
                    if readiness.pty_error {
                        self.handle_write_failure(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "poll encountered PTY fd error",
                        ));
                        break;
                    }
                    if readiness.pty_read_ready && !self.read_once() {
                        break;
                    }
                    if readiness.pty_write_ready
                        && let Err(err) = self.pump()
                    {
                        self.handle_write_failure(err);
                        break;
                    }
                }
                Err(err) => {
                    debug!(pane = self.pane_id.raw(), err = %err, "PTY actor poll failed");
                    break;
                }
            }
        }

        self.close_inbox();
        if let Some(on_reader_exit) = self.on_reader_exit.take() {
            on_reader_exit(if self.read_callback_panicked {
                ReaderExit::Panicked
            } else {
                ReaderExit::Closed
            });
        }
        debug!(pane = self.pane_id.raw(), "PTY actor exiting");
    }

    /// Write what the PTY takes now, handling the non-write work (resizes,
    /// submission starts, Enter scheduling, withdrawals) around each write,
    /// so nothing that is ready waits for the next wake or the idle poll.
    fn pump(&mut self) -> std::io::Result<()> {
        for _ in 0..MAX_WRITE_STEPS_PER_PUMP {
            self.withdraw_cancelled_submission();
            self.advance_inbox();
            match self.write_next()? {
                WriteStep::Idle | WriteStep::Blocked => return Ok(()),
                WriteStep::Progress(Some(part)) => self.complete_submission_part(part),
                WriteStep::Progress(None) => {}
            }
        }
        // Out of steps with the PTY still writable: the next poll returns at
        // once. Settle the non-write work first so it is not left waiting.
        self.withdraw_cancelled_submission();
        self.advance_inbox();
        Ok(())
    }

    /// Handle every non-write inbox event that is ready. Each step either
    /// changes the actor's state or reports nothing to do, so this ends.
    fn advance_inbox(&mut self) {
        while self.process_next_inbox_event() || self.schedule_submission_enter() {}
    }

    /// Apply a due resize, or start the submission at the front of the
    /// queue. Returns whether anything changed.
    fn process_next_inbox_event(&mut self) -> bool {
        if self.apply_due_resize() {
            return true;
        }
        // One submission at a time; the rest wait in queue order.
        if self.active_submission.is_some() {
            return false;
        }
        let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
        let Some(entry_index) = inbox.next_entry_index(false, self.current_write_order) else {
            return false;
        };
        let Some(entry) = inbox.entries.get(entry_index) else {
            return false;
        };
        if !matches!(entry.kind, PtyIoInboxEntryKind::Submission { .. })
            || inbox.resize_holds(entry.order)
        {
            return false;
        }
        let Some(PtyIoInboxEntry { order, kind }) = inbox.entries.remove(entry_index) else {
            return false;
        };
        let (text, enter, delay, reply, state) = match kind {
            PtyIoInboxEntryKind::Submission {
                text,
                enter,
                delay,
                reply,
                state,
            } => (text, enter, delay, reply, state),
            kind => {
                inbox
                    .entries
                    .insert(entry_index, PtyIoInboxEntry { order, kind });
                return false;
            }
        };
        if !lock_state(&state).start(text.is_empty(), delay) {
            lock_state(&state).finish();
            inbox.release_bytes(text.len().saturating_add(enter.len()));
            inbox.release_item();
            drop(inbox);
            deliver_submission_result(&reply, Err(submission_withdrawn_error()));
            return true;
        }

        // The text takes the submission's place in the sequence and keeps
        // its reservation; the Enter's bytes stay reserved until it is queued.
        if !text.is_empty() {
            inbox.entries.insert(
                entry_index,
                PtyIoInboxEntry {
                    order,
                    kind: PtyIoInboxEntryKind::Write(PendingWrite::Submission {
                        bytes: text,
                        part: SubmissionPart::Text,
                    }),
                },
            );
        }
        drop(inbox);
        self.active_submission = Some(ActiveSubmission {
            unqueued_enter_bytes: enter.len(),
            enter,
            reply,
            state,
        });
        true
    }

    /// Apply the pending resize if it is due. The ioctl runs as soon as the
    /// request arrives, not behind queued writes: a child that is not reading
    /// stdin must still get its SIGWINCH. Only the replies take the request's
    /// place in the sequence. The runtime has already resized the emulator,
    /// so a failed ioctl is retried with backoff until it succeeds or a newer
    /// request replaces it. Returns whether anything changed.
    fn apply_due_resize(&mut self) -> bool {
        let (order, resize) = {
            let inbox = crate::locks::lock_auxiliary(&self.inbox);
            let Some(pending) = inbox.latest_resize.as_ref() else {
                return false;
            };
            if pending
                .retry_at
                .is_some_and(|retry_at| retry_at > Instant::now())
            {
                return false;
            }
            (pending.order, pending.resize)
        };
        let result = (self.resize_pty)(self.file.as_raw_fd(), resize);

        let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
        if inbox
            .latest_resize
            .as_ref()
            .is_none_or(|pending| pending.order != order)
        {
            // Replaced during the ioctl; the newer request is applied next.
            return true;
        }
        match result {
            Ok(()) => {
                if let Some(done) = inbox.latest_resize.take() {
                    if done.attempts > 0 {
                        debug!(
                            pane = self.pane_id.raw(),
                            attempts = done.attempts,
                            "PTY resize applied after retrying"
                        );
                    }
                    inbox.insert_resize_replies(done.order, done.terminal_responses);
                }
                true
            }
            Err(err) => {
                let Some(pending) = inbox.latest_resize.as_mut() else {
                    return false;
                };
                pending.attempts = pending.attempts.saturating_add(1);
                let attempts = pending.attempts;
                let shift = u32::from(attempts.saturating_sub(1).min(7));
                let delay = RESIZE_RETRY_BASE
                    .saturating_mul(1u32 << shift)
                    .min(RESIZE_RETRY_MAX);
                pending.retry_at = Some(Instant::now() + delay);
                // Holding the replies keeps them ordered, but it also holds
                // every write queued after them. Past a few attempts the
                // replies go out without the ioctl (they describe the
                // emulator, which has the new size) and input flows again.
                let released = if attempts >= RESIZE_HOLD_ATTEMPTS {
                    std::mem::take(&mut pending.terminal_responses)
                } else {
                    Vec::new()
                };
                let released_any = !released.is_empty();
                inbox.insert_resize_replies(order, released);
                drop(inbox);
                if attempts == 1 {
                    warn!(
                        pane = self.pane_id.raw(),
                        err = %err,
                        "PTY resize failed; retrying with backoff"
                    );
                } else if released_any {
                    warn!(
                        pane = self.pane_id.raw(),
                        err = %err,
                        attempts,
                        "PTY resize still failing; releasing its replies and retrying"
                    );
                } else {
                    debug!(
                        pane = self.pane_id.raw(),
                        err = %err,
                        attempts,
                        "PTY resize retry failed"
                    );
                }
                released_any
            }
        }
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
        // Replies generated while draining are discarded when the inbox closes.
        self.current_write_order = None;
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
                // Held across parsing and queuing this read's replies; see
                // `PtyIoActorHandle::response_order`.
                let response_order = Arc::clone(&self.response_order);
                let _order = crate::locks::lock_auxiliary(&response_order);
                let on_read = &mut self.on_read;
                let bytes = &buf[..n];
                #[expect(
                    clippy::disallowed_methods,
                    reason = "a panic in the terminal core must not unwind out of the actor \
                              thread: that would skip the reader-exit report and leave the pane \
                              dead with nobody told. The panic is logged and the pane closed. \
                              Catching it costs nothing on the non-panicking path"
                )]
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
                let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
                if !inbox.shutdown {
                    for response in result.terminal_responses {
                        let _ = inbox.push_terminal_response(response);
                    }
                }
                ReadOutcome::Data
            }
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

    /// End the active submission and release everything it reserved,
    /// including any of its parts still queued (a withdrawn part, or one left
    /// behind by a write failure).
    fn finish_active_submission(&mut self, result: std::io::Result<()>) {
        let Some(submission) = self.active_submission.take() else {
            return;
        };
        lock_state(&submission.state).finish();
        {
            let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
            while let Some(index) = inbox.entries.iter().position(|entry| {
                matches!(
                    entry.kind,
                    PtyIoInboxEntryKind::Write(PendingWrite::Submission { .. })
                )
            }) {
                let order = inbox.entries.get(index).map(|entry| entry.order);
                let written = if order.is_some() && order == self.current_write_order {
                    self.current_write_order = None;
                    std::mem::take(&mut self.current_write_offset)
                } else {
                    0
                };
                inbox.remove_entry(index, written);
            }
            inbox.release_bytes(submission.unqueued_enter_bytes);
            inbox.release_item();
        }
        deliver_submission_result(&submission.reply, result);
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
        if should_withdraw {
            // The unstarted part still queued is removed with the submission.
            self.finish_active_submission(Err(submission_withdrawn_error()));
        }
    }

    /// Queue the Enter once the delay after the text has passed. Returns
    /// whether anything changed.
    fn schedule_submission_enter(&mut self) -> bool {
        let Some(submission) = self.active_submission.as_ref() else {
            return false;
        };
        let enter = submission.enter.clone();
        let state = Arc::clone(&submission.state);
        let start = lock_state(&state).start_enter(Instant::now(), enter.is_empty());
        match start {
            EnterStart::Cancelled => {
                self.finish_active_submission(Err(submission_withdrawn_error()));
                true
            }
            EnterStart::Empty => {
                self.finish_active_submission(Ok(()));
                true
            }
            EnterStart::Started => {
                if !enter.is_empty() {
                    let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
                    let order = inbox.next_order();
                    inbox.entries.push_back(PtyIoInboxEntry {
                        order,
                        kind: PtyIoInboxEntryKind::Write(PendingWrite::Submission {
                            bytes: enter,
                            part: SubmissionPart::Enter,
                        }),
                    });
                }
                if let Some(submission) = self.active_submission.as_mut() {
                    submission.unqueued_enter_bytes = 0;
                }
                true
            }
            EnterStart::NotReady => false,
        }
    }

    fn poll_timeout_ms(&self) -> i32 {
        let now = Instant::now();
        let submission_deadline = self
            .active_submission
            .as_ref()
            .and_then(|submission| lock_state(&submission.state).deadline());
        let resize_deadline = crate::locks::lock_auxiliary(&self.inbox)
            .latest_resize
            .as_ref()
            .and_then(|resize| resize.retry_at);
        let deadline = match (submission_deadline, resize_deadline) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => None,
        };
        let Some(deadline) = deadline else {
            return ACTOR_IDLE_POLL_MS;
        };
        i32::try_from(
            deadline
                .saturating_duration_since(now)
                .as_millis()
                .max(1)
                .min(ACTOR_IDLE_POLL_MS as u128),
        )
        .unwrap_or(ACTOR_IDLE_POLL_MS)
    }

    fn fail_active_submission(&mut self, err: std::io::Error) {
        self.finish_active_submission(Err(err));
    }

    fn close_inbox(&mut self) {
        let mut queued_submissions = Vec::new();
        {
            let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
            inbox.shutdown = true;
            for entry in inbox.entries.drain(..) {
                if let PtyIoInboxEntryKind::Submission { reply, state, .. } = entry.kind {
                    queued_submissions.push((reply, state));
                }
            }
            inbox.latest_resize = None;
            inbox.pending_bytes = 0;
            inbox.pending_items = 0;
        }
        let error = input_submission_closed_error();
        if let Some(submission) = self.active_submission.take() {
            lock_state(&submission.state).finish();
            deliver_submission_result(
                &submission.reply,
                Err(std::io::Error::new(error.kind(), error.to_string())),
            );
        }
        for (reply, state) in queued_submissions {
            lock_state(&state).finish();
            deliver_submission_result(
                &reply,
                Err(std::io::Error::new(error.kind(), error.to_string())),
            );
        }
    }

    /// Take one write step on the next writable entry. The inbox lock is
    /// released around the write syscall; the entry's index stays valid
    /// because only this thread removes or inserts entries.
    fn write_next(&mut self) -> std::io::Result<WriteStep> {
        let (index, order, bytes, part) = {
            let inbox = crate::locks::lock_auxiliary(&self.inbox);
            let Some(index) =
                inbox.writable_index(self.active_submission.is_some(), self.current_write_order)
            else {
                return Ok(WriteStep::Idle);
            };
            let Some(entry) = inbox.entries.get(index) else {
                return Ok(WriteStep::Idle);
            };
            let PtyIoInboxEntryKind::Write(write) = &entry.kind else {
                return Ok(WriteStep::Idle);
            };
            let (bytes, part) = match write {
                PendingWrite::User(bytes) | PendingWrite::TerminalResponse(bytes) => {
                    (bytes.clone(), None)
                }
                PendingWrite::Submission { bytes, part } => (bytes.clone(), Some(*part)),
            };
            (index, entry.order, bytes, part)
        };
        let offset = self.current_write_offset;

        // The first byte of a submission part is written under the state
        // lock, so a cancel either skips the whole part or sees that it has
        // started. Later chunks finish without it: cancellation never
        // truncates a part in flight.
        let state = match part {
            Some(_) if offset == 0 => self
                .active_submission
                .as_ref()
                .map(|submission| Arc::clone(&submission.state)),
            _ => None,
        };
        let mut state_guard = state.as_deref().map(lock_state);
        let skip = match (part, offset, state_guard.as_ref()) {
            // A part whose submission has already ended.
            (Some(_), 0, None) => true,
            (Some(part), 0, Some(guard)) => !guard.can_write_first_byte(part),
            _ => false,
        };
        if skip {
            drop(state_guard);
            self.retire_entry(index, order, 0);
            return Ok(WriteStep::Progress(part));
        }

        let write_result = self.file.write(&bytes[offset..]);
        match write_result {
            Ok(0) => Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "PTY actor write returned zero bytes",
            )),
            Ok(written) => {
                if let (Some(guard), Some(part)) = (state_guard.as_mut(), part) {
                    guard.first_byte_written(part);
                }
                drop(state_guard);
                let offset = offset.saturating_add(written);
                if offset < bytes.len() {
                    crate::locks::lock_auxiliary(&self.inbox).release_bytes(written);
                    self.current_write_order = Some(order);
                    self.current_write_offset = offset;
                    return Ok(WriteStep::Progress(None));
                }
                // `retire_entry` releases what the earlier chunks left.
                self.retire_entry(index, order, offset.saturating_sub(written));
                if part.is_some() {
                    self.file.flush()?;
                }
                Ok(WriteStep::Progress(part))
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(WriteStep::Blocked),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                Ok(WriteStep::Progress(None))
            }
            Err(err) => {
                warn!(pane = self.pane_id.raw(), err = %err, "PTY actor write failed");
                Err(err)
            }
        }
    }

    /// Remove a finished or skipped entry. `released` bytes of it were
    /// released as earlier chunks were written.
    fn retire_entry(&mut self, index: usize, order: u64, released: usize) {
        self.current_write_order = None;
        self.current_write_offset = 0;
        let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
        if inbox
            .entries
            .get(index)
            .is_some_and(|entry| entry.order == order)
        {
            inbox.remove_entry(index, released);
        }
    }

    fn has_writable_work(&self) -> bool {
        crate::locks::lock_auxiliary(&self.inbox)
            .writable_index(self.active_submission.is_some(), self.current_write_order)
            .is_some()
    }
}

fn resize_pty(fd: RawFd, resize: PtyResize) -> std::io::Result<()> {
    fd::resize_pty_fd(
        fd,
        resize.geometry.rows(),
        resize.geometry.cols(),
        resize.geometry.cell_width(),
        resize.geometry.cell_height(),
    )
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

/// Hand a submission's outcome to whoever queued it. The receiver is the
/// `QueuedSubmission::completion` its caller holds; it is gone only when the
/// caller dropped the submission, and then nobody is waiting for the outcome,
/// so a failed send is not an error.
fn deliver_submission_result(
    reply: &std_mpsc::Sender<std::io::Result<()>>,
    result: std::io::Result<()>,
) {
    reply.send(result).ok();
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

    fn actor_test_parts(on_read: ReadCallback) -> (PtyIoActorRunner, PtyIoActorHandle, UnixStream) {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        // SAFETY: into_raw_fd transfers this socket's sole fd ownership to OwnedFd.
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        let inbox = Arc::new(Mutex::new(PtyIoInbox::default()));
        let response_order = Arc::new(Mutex::new(()));
        let handle = PtyIoActorHandle {
            wake: wake_pipe.writer,
            inbox: Arc::clone(&inbox),
            response_order: Arc::clone(&response_order),
        };
        let runner = PtyIoActorRunner {
            pane_id: PaneId::from_raw(1),
            file: std::fs::File::from(owned),
            inbox,
            response_order,
            current_write_order: None,
            current_write_offset: 0,
            active_submission: None,
            wake_read_fd: wake_pipe.read_fd,
            on_read,
            on_reader_exit: None,
            core_broken: None,
            read_callback_panicked: false,
            poll_observer: None,
            resize_pty: Box::new(resize_pty),
        };
        (runner, handle, peer)
    }

    fn actor_runner_for_unit_test() -> (PtyIoActorRunner, UnixStream) {
        let (runner, _handle, peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        (runner, peer)
    }

    #[test]
    fn write_failure_still_delivers_the_childs_last_output() {
        let (read_tx, read_rx) = std_mpsc::channel();
        let (mut runner, _handle, mut peer) = actor_test_parts(Box::new(move |bytes| {
            read_tx
                .send(Bytes::copy_from_slice(bytes))
                .expect("the test holds the read receiver while the runner runs");
            PtyReadResult::empty()
        }));
        // The child prints its last words and exits with a reply still queued.
        peer.write_all(b"last-output").expect("peer write");
        drop(peer);
        crate::locks::lock_auxiliary(&runner.inbox)
            .push_user_input(Bytes::from_static(b"queued-reply"))
            .expect("test write fits inbox");

        runner.run();

        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("buffered output is read after the write fails");
        assert_eq!(read, Bytes::from_static(b"last-output"));
    }

    #[test]
    fn rejected_user_input_hands_its_bytes_back() {
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            wake,
            inbox: Arc::new(Mutex::new(PtyIoInbox::default())),
            response_order: Arc::new(Mutex::new(())),
        };
        handle
            .try_write_user_input(Bytes::from(vec![b'f'; ACTOR_INBOX_MAX_BYTES]))
            .expect("first write fits the queue");
        match handle.try_write_user_input(Bytes::from_static(b"full")) {
            Err(TrySendError::Full(bytes)) => assert_eq!(bytes, "full"),
            other => panic!("expected a full queue, got {other:?}"),
        }
        handle.shutdown();
        match handle.try_write_user_input(Bytes::from_static(b"closed")) {
            Err(TrySendError::Closed(bytes)) => assert_eq!(bytes, "closed"),
            other => panic!("expected a closed queue, got {other:?}"),
        }
    }

    #[test]
    fn unread_child_stdin_keeps_input_bounded_and_returns_full() {
        let (mut actor_socket, _peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        assert!(fill_send_buffer(&mut actor_socket) > 0);
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

        let chunk = Bytes::from(vec![b'x'; 16 * 1024]);
        let mut rejected = None;
        for _ in 0..(ACTOR_INBOX_MAX_BYTES / chunk.len() + 8) {
            match handle.try_write_user_input(chunk.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(bytes)) => {
                    rejected = Some(bytes);
                    break;
                }
                Err(TrySendError::Closed(_)) => panic!("live actor closed unexpectedly"),
            }
        }

        assert_eq!(rejected, Some(chunk));
        let inbox = crate::locks::lock_auxiliary(&handle.inbox);
        assert!(inbox.pending_bytes <= ACTOR_INBOX_MAX_BYTES);
        assert!(inbox.pending_bytes > 0, "unread input remains queued");
        assert!(inbox.pending_items <= ACTOR_INBOX_MAX_ITEMS);
        drop(inbox);
        handle.shutdown();
    }

    #[test]
    fn actor_ignores_empty_user_input_write() {
        let (runner, _peer) = actor_runner_for_unit_test();
        let inbox = crate::locks::lock_auxiliary(&runner.inbox);
        assert_eq!(inbox.pending_bytes, 0);
        drop(inbox);
        let result = crate::locks::lock_auxiliary(&runner.inbox).push_user_input(Bytes::new());
        assert!(result.is_ok());
        assert!(
            crate::locks::lock_auxiliary(&runner.inbox)
                .entries
                .is_empty()
        );
    }

    #[test]
    fn submission_part_does_not_wait_for_following_protocol_write() {
        let (mut runner, mut peer) = actor_runner_for_unit_test();
        let state = SubmissionState::shared();
        assert!(lock_state(&state).start(false, Duration::ZERO));
        let (reply, _completion) = std_mpsc::channel();
        runner.active_submission = Some(ActiveSubmission {
            enter: Bytes::new(),
            unqueued_enter_bytes: 0,
            reply,
            state,
        });
        {
            let mut inbox = crate::locks::lock_auxiliary(&runner.inbox);
            inbox.reserve(14, 2);
            let submission_order = inbox.next_order();
            inbox.entries.push_back(PtyIoInboxEntry {
                order: submission_order,
                kind: PtyIoInboxEntryKind::Write(PendingWrite::Submission {
                    bytes: Bytes::from_static(b"prompt"),
                    part: SubmissionPart::Text,
                }),
            });
            let reply_order = inbox.next_order();
            inbox.entries.push_back(PtyIoInboxEntry {
                order: reply_order,
                kind: PtyIoInboxEntryKind::Write(PendingWrite::TerminalResponse(
                    Bytes::from_static(b"response"),
                )),
            });
        }

        assert_eq!(
            runner.write_next().expect("test precondition"),
            WriteStep::Progress(Some(SubmissionPart::Text))
        );
        let mut prompt = [0; 6];
        peer.read_exact(&mut prompt)
            .expect("prompt was written first");
        assert_eq!(&prompt, b"prompt");
        let inbox = crate::locks::lock_auxiliary(&runner.inbox);
        assert!(matches!(
            inbox.entries.front().map(|entry| &entry.kind),
            Some(PtyIoInboxEntryKind::Write(PendingWrite::TerminalResponse(bytes)))
                if bytes.as_ref() == b"response"
        ));
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
        let text = Bytes::from(vec![b'x'; 240 * 1024]);
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
                    let handle = crate::locks::lock_auxiliary(&handle_slot)
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
        *crate::locks::lock_auxiliary(&handle_slot) = Some(handle);

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
                // The actor thread can outlive a test that already finished
                // and dropped the receiver; tests that check the exit hold it.
                exit_tx.send(exit).ok();
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
    fn resize_keeps_latest_request_without_reordering_queued_replies() {
        let (_runner, handle, _peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        handle.write_terminal_response(|| Some(Bytes::from_static(b"before")));
        handle.resize(
            shepr_core::geometry::PaneGeometry::new(80, 20, 8, 16),
            || vec![Bytes::from_static(b"old")],
        );
        handle.write_terminal_response(|| Some(Bytes::from_static(b"middle")));
        handle.resize(
            shepr_core::geometry::PaneGeometry::new(120, 40, 9, 18),
            || vec![Bytes::from_static(b"new")],
        );

        let inbox = crate::locks::lock_auxiliary(&handle.inbox);
        assert_eq!(
            inbox
                .latest_resize
                .as_ref()
                .map(|resize| resize.resize.geometry),
            Some(shepr_core::geometry::PaneGeometry::new(120, 40, 9, 18))
        );
        assert_eq!(
            inbox
                .latest_resize
                .as_ref()
                .map(|resize| resize.terminal_responses.as_slice()),
            Some(&[Bytes::from_static(b"new")][..])
        );
        let responses: Vec<_> = inbox
            .entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                PtyIoInboxEntryKind::Write(PendingWrite::TerminalResponse(bytes)) => {
                    Some(bytes.as_ref())
                }
                _ => None,
            })
            .collect();
        assert_eq!(responses, [b"before".as_slice(), b"middle".as_slice()]);
    }

    #[test]
    fn appearance_transition_report_precedes_query_of_new_scheme() {
        let light = Arc::new(AtomicBool::new(false));
        let query_light = Arc::clone(&light);
        let (runner, handle, mut peer) = actor_test_parts(Box::new(move |_| PtyReadResult {
            terminal_responses: vec![if query_light.load(Ordering::Acquire) {
                Bytes::from_static(b"query-light")
            } else {
                Bytes::from_static(b"query-dark")
            }],
            core_broken: false,
        }));
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

        let inbox = crate::locks::lock_auxiliary(&runner.inbox);
        let responses: Vec<_> = inbox
            .entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                PtyIoInboxEntryKind::Write(PendingWrite::TerminalResponse(bytes)) => {
                    Some(bytes.as_ref())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            responses,
            [b"live-light".as_slice(), b"query-light".as_slice()]
        );
    }

    #[test]
    fn resize_writes_terminal_responses_after_applying_resize() {
        let (mut runner, handle, mut peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        runner.resize_pty = Box::new(|_, _| Ok(()));
        handle.write_terminal_response(|| Some(Bytes::from_static(b"earlier")));
        handle.resize(
            shepr_core::geometry::PaneGeometry::new(100, 40, 9, 18),
            || vec![Bytes::from_static(b"resize")],
        );
        handle.write_terminal_response(|| Some(Bytes::from_static(b"later")));

        runner.pump().expect("queued response writes");

        let mut bytes = [0; 18];
        peer.read_exact(&mut bytes)
            .expect("peer receives replies in inbox order");
        assert_eq!(&bytes, b"earlierresizelater");
    }

    #[test]
    fn failed_resize_is_retried_without_dropping_its_reply() {
        let (mut runner, handle, mut peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let resize_calls = Arc::clone(&calls);
        runner.resize_pty = Box::new(move |_, _| {
            if resize_calls.fetch_add(1, Ordering::AcqRel) == 0 {
                Err(std::io::Error::other("temporary ioctl failure"))
            } else {
                Ok(())
            }
        });
        handle.resize(
            shepr_core::geometry::PaneGeometry::new(100, 40, 9, 18),
            || vec![Bytes::from_static(b"resize-reply")],
        );
        handle.write_terminal_response(|| Some(Bytes::from_static(b"later")));

        runner.pump().expect("pump with a failing resize");
        {
            let mut inbox = crate::locks::lock_auxiliary(&runner.inbox);
            let resize = inbox
                .latest_resize
                .as_mut()
                .expect("failed resize stays queued");
            assert_eq!(resize.attempts, 1);
            assert!(resize.retry_at.is_some());
            assert_eq!(
                resize.terminal_responses,
                [Bytes::from_static(b"resize-reply")]
            );
            resize.retry_at = Some(Instant::now());
        }
        // The later reply waits behind the held one.
        peer.set_nonblocking(true).expect("peer nonblocking");
        let mut probe = [0u8; 1];
        assert!(
            peer.read(&mut probe).is_err(),
            "nothing overtakes a held resize reply"
        );
        peer.set_nonblocking(false).expect("peer blocking");

        runner.pump().expect("resize reply write succeeds");
        assert_eq!(calls.load(Ordering::Acquire), 2);
        assert!(
            crate::locks::lock_auxiliary(&runner.inbox)
                .latest_resize
                .is_none()
        );
        let mut response = [0; 17];
        peer.read_exact(&mut response)
            .expect("reply follows successful resize retry");
        assert_eq!(&response, b"resize-replylater");
    }

    #[test]
    fn a_resize_that_keeps_failing_stops_holding_input_and_a_newer_one_replaces_it() {
        let (mut runner, handle, mut peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        let succeed = Arc::new(AtomicBool::new(false));
        let applied = Arc::new(Mutex::new(Vec::new()));
        runner.resize_pty = Box::new({
            let succeed = Arc::clone(&succeed);
            let applied = Arc::clone(&applied);
            move |_, resize| {
                if succeed.load(Ordering::Acquire) {
                    crate::locks::lock_auxiliary(&applied).push(resize.geometry);
                    Ok(())
                } else {
                    Err(std::io::Error::other("persistent ioctl failure"))
                }
            }
        });
        handle.resize(
            shepr_core::geometry::PaneGeometry::new(100, 40, 9, 18),
            || vec![Bytes::from_static(b"reply")],
        );
        handle
            .try_write_user_input(Bytes::from_static(b"typed"))
            .expect("input queues");

        for attempt in 1..=RESIZE_HOLD_ATTEMPTS {
            runner.pump().expect("pump with a failing resize");
            let mut inbox = crate::locks::lock_auxiliary(&runner.inbox);
            let resize = inbox
                .latest_resize
                .as_mut()
                .expect("failed resize stays queued");
            assert_eq!(resize.attempts, attempt);
            let retry_in = resize
                .retry_at
                .expect("failure schedules a retry")
                .saturating_duration_since(Instant::now());
            assert!(retry_in > Duration::ZERO && retry_in <= RESIZE_RETRY_MAX);
            resize.retry_at = Some(Instant::now());
        }
        // Released: the reply and the input behind it go out, the ioctl is
        // still pending.
        let mut received = [0; 10];
        peer.read_exact(&mut received)
            .expect("held reply and input are released");
        assert_eq!(&received, b"replytyped");
        assert!(
            crate::locks::lock_auxiliary(&runner.inbox)
                .latest_resize
                .as_ref()
                .is_some_and(|resize| resize.terminal_responses.is_empty()),
            "the ioctl keeps retrying after the replies are released"
        );

        let newer = shepr_core::geometry::PaneGeometry::new(120, 50, 9, 18);
        handle.resize(newer, Vec::new);
        succeed.store(true, Ordering::Release);
        runner.pump().expect("newer resize applies");
        assert_eq!(*crate::locks::lock_auxiliary(&applied), [newer]);
        assert!(
            crate::locks::lock_auxiliary(&runner.inbox)
                .latest_resize
                .is_none()
        );
    }

    #[test]
    fn resize_is_applied_while_earlier_input_waits_on_an_unread_pty() {
        let (mut runner, handle, _peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        runner.resize_pty = Box::new({
            let calls = Arc::clone(&calls);
            move |_, _| {
                calls.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }
        });
        // More than the socket buffer holds; the peer never reads.
        handle
            .try_write_user_input(Bytes::from(vec![b'x'; 4 * 1024 * 1024]))
            .expect("one oversized write enters an empty inbox");
        runner.pump().expect("fill the PTY");
        assert!(
            !crate::locks::lock_auxiliary(&runner.inbox)
                .entries
                .is_empty(),
            "the write is still waiting on the PTY"
        );

        handle.resize(
            shepr_core::geometry::PaneGeometry::new(100, 40, 9, 18),
            Vec::new,
        );
        runner.pump().expect("pump with blocked input");
        assert_eq!(
            calls.load(Ordering::Acquire),
            1,
            "the child gets its SIGWINCH without reading stdin first"
        );
    }

    #[test]
    fn oversized_input_is_admitted_only_into_an_empty_inbox() {
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            wake,
            inbox: Arc::new(Mutex::new(PtyIoInbox::default())),
            response_order: Arc::new(Mutex::new(())),
        };
        let paste = Bytes::from(vec![b'p'; ACTOR_INBOX_MAX_BYTES * 2]);
        handle
            .try_write_user_input(paste.clone())
            .expect("a large paste reaches an idle pane");
        match handle.try_write_user_input(Bytes::from_static(b"k")) {
            Err(TrySendError::Full(bytes)) => assert_eq!(bytes, "k"),
            other => panic!("expected a full queue, got {other:?}"),
        }
        match handle.try_write_user_input(paste) {
            Err(TrySendError::Full(bytes)) => assert_eq!(bytes.len(), ACTOR_INBOX_MAX_BYTES * 2),
            other => panic!("expected a full queue, got {other:?}"),
        }
    }

    #[test]
    fn unread_child_stdin_bounds_terminal_replies() {
        let (mut actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        assert!(fill_send_buffer(&mut actor_socket) > 0);
        // SAFETY: into_raw_fd transfers this socket's sole fd ownership to OwnedFd.
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        const REPLY_LEN: usize = 4096;
        let (read_tx, read_rx) = std_mpsc::channel();
        let handle = PtyIoActor::spawn(PtyIoActorConfig {
            pane_id: PaneId::from_raw(1),
            master_fd: owned,
            // Every read is a query that earns a reply, as for a child that
            // prints DA1 or DSR in a loop.
            on_read: Box::new(move |bytes| {
                // The actor thread keeps reading after the test has counted
                // enough and dropped the receiver; later reads need no count.
                read_tx.send(bytes.len()).ok();
                PtyReadResult {
                    terminal_responses: vec![Bytes::from(vec![b'r'; REPLY_LEN])],
                    core_broken: false,
                }
            }),
            on_reader_exit: None,
            core_broken: None,
        })
        .expect("actor spawn");

        // The child never reads its stdin; it only prints queries.
        const QUERY_BYTES: usize = 4 * 1024 * 1024;
        let mut writer = peer.try_clone().expect("clone peer writer");
        let query_writer = std::thread::spawn(move || {
            let chunk = vec![b'q'; 1024];
            for _ in 0..QUERY_BYTES / chunk.len() {
                writer.write_all(&chunk).expect("child prints queries");
            }
        });
        let mut read = 0;
        while read < QUERY_BYTES {
            read += read_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("actor keeps reading queries");
        }
        query_writer.join().expect("query writer joins");

        let inbox = crate::locks::lock_auxiliary(&handle.inbox);
        assert!(inbox.pending_bytes <= ACTOR_INBOX_MAX_BYTES);
        assert!(inbox.pending_items <= ACTOR_INBOX_MAX_ITEMS);
        let queued: usize = inbox
            .entries
            .iter()
            .map(|entry| match &entry.kind {
                PtyIoInboxEntryKind::Write(PendingWrite::TerminalResponse(bytes)) => bytes.len(),
                _ => 0,
            })
            .sum();
        assert!(
            queued <= ACTOR_INBOX_MAX_BYTES,
            "queued {queued} reply bytes"
        );
        assert!(
            queued > ACTOR_INBOX_MAX_BYTES - 2 * REPLY_LEN,
            "replies fill the inbox up to its bound"
        );
        drop(inbox);
        handle.shutdown();
        drop(peer);
    }

    #[test]
    fn submission_queued_behind_input_starts_without_waiting_for_the_idle_poll() {
        let (runner, handle, mut peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        handle
            .try_write_user_input(Bytes::from_static(b"focus"))
            .expect("input queues");
        let completion = handle
            .queue_user_input_submission(
                Bytes::from_static(b"prompt"),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
            )
            .expect("submission queues")
            .completion;
        // Both wakes were consumed by an earlier poll, as when they land
        // while the actor is already awake.
        fd::drain_wake_fd(runner.wake_read_fd.as_raw_fd()).expect("drain wake");

        let started = Instant::now();
        let actor = std::thread::spawn(move || {
            let mut runner = runner;
            runner.run();
        });
        peer.set_read_timeout(Some(Duration::from_secs(2)))
            .expect("peer timeout");
        let mut received = [0; 12];
        peer.read_exact(&mut received)
            .expect("peer receives input then the prompt");
        assert_eq!(&received, b"focusprompt\r");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the submission must not wait for the idle poll"
        );
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports submission")
            .expect("submission completes");
        handle.shutdown();
        actor.join().expect("actor joins");
    }

    #[test]
    fn finished_and_withdrawn_submissions_release_their_reservation() {
        let (mut runner, handle, mut peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        let queued = handle
            .queue_user_input_submission(
                Bytes::from_static(b"prompt"),
                Bytes::from_static(b"\r"),
                Duration::from_secs(5),
            )
            .expect("submission queues");
        runner.pump().expect("text writes");
        let mut prompt = [0; 6];
        peer.read_exact(&mut prompt).expect("peer receives prompt");
        assert_eq!(
            queued.cancel.cancel(),
            SubmissionCancelOutcome::TextUnsubmitted
        );
        runner.pump().expect("withdraw the Enter");
        queued
            .completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports the cancel")
            .expect_err("a cancelled submission does not complete");
        {
            let inbox = crate::locks::lock_auxiliary(&runner.inbox);
            assert_eq!((inbox.pending_bytes, inbox.pending_items), (0, 0));
        }

        let completion = handle
            .queue_user_input_submission(
                Bytes::from_static(b"again"),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
            )
            .expect("submission queues")
            .completion;
        runner.pump().expect("submission writes");
        let mut written = [0; 6];
        peer.read_exact(&mut written)
            .expect("peer receives submission");
        assert_eq!(&written, b"again\r");
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports submission")
            .expect("submission completes");
        let inbox = crate::locks::lock_auxiliary(&runner.inbox);
        assert_eq!((inbox.pending_bytes, inbox.pending_items), (0, 0));
        assert!(inbox.entries.is_empty());
    }
}
