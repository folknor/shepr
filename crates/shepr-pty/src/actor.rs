use std::{
    collections::VecDeque,
    io::{Read, Write},
    os::fd::{AsRawFd, OwnedFd, RawFd},
    sync::{Arc, Mutex},
};

use bytes::Bytes;
use shepr_core::layout::PaneId;
use shepr_platform::Wait;
use tracing::{debug, error, warn};

use crate::{
    child_io::ChildIoSendError,
    fd,
    limits::{
        ACTOR_IDLE_POLL, ACTOR_INBOX_MAX_BYTES, ACTOR_INBOX_MAX_ITEMS,
        MAX_WRITE_FAILURE_DRAIN_CHUNKS, MAX_WRITE_STEPS_PER_PUMP, PTY_READ_BUFFER_BYTES,
    },
};

/// Effects from a PTY read that the actor can deliver to the child.
pub struct PtyReadEffects {
    /// Replies generated while parsing this read.
    pub terminal_responses: Vec<Bytes>,
    /// Effects that may block or call into other subsystems. The actor queues
    /// `terminal_responses` under the reply-order lock, then runs these only
    /// after releasing it.
    pub after_response_order: Option<Box<dyn FnOnce() + Send + 'static>>,
}

pub enum PtyReadResult {
    /// The read was parsed and produced these effects.
    Effects(PtyReadEffects),
    /// The callback cannot consume this or any later bytes because the
    /// terminal core's lock was poisoned by a panic on another thread.
    CoreBroken,
}

type ReadCallback = Box<dyn FnMut(&[u8]) -> PtyReadResult + Send + 'static>;
type ReaderExitCallback = Box<dyn FnOnce(ReaderExit) + Send + 'static>;
/// Whether the terminal core has been broken by a panic on some other thread.
/// Must be cheap (an atomic load): the actor asks on every loop iteration.
type CoreBrokenCheck = Box<dyn Fn() -> bool + Send + 'static>;

/// Why the actor's IO loop ended. Ranked by explicit severity: when the loop sees
/// more than one ending (a write failure, then EIO while draining the
/// child's last output), the most severe one is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderExit {
    /// The owner asked the actor to stop; the pane is already being torn down.
    ShutdownRequested,
    /// EOF or EIO: every holder of the PTY slave closed it. That is how a
    /// pane normally ends, but it is not proof the child has exited: a child
    /// can close its terminal and keep running.
    Closed,
    /// A hard PTY read, write, poll or wake-pipe failure, or a PTY error with
    /// nothing to read. The child may still be running, so the owner must
    /// remove the pane and tear down its session rather than waiting for the
    /// child watcher.
    IoFailed,
    /// The read callback panicked, or reported the terminal core broken by a
    /// panic elsewhere (a terminal core bug either way). The loop stops and
    /// the master fd is closed, but the child may outlive the SIGHUP, so the
    /// owner must be told the pane is dead.
    Panicked,
}

impl ReaderExit {
    fn severity(self) -> u8 {
        match self {
            Self::ShutdownRequested => 0,
            Self::Closed => 1,
            Self::IoFailed => 2,
            Self::Panicked => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PtyResize {
    geometry: shepr_core::geometry::PaneGeometry,
}

pub struct PtyIoActorConfig {
    pub pane_id: PaneId,
    pub master_fd: OwnedFd,
    pub on_read: ReadCallback,
    pub on_reader_exit: ReaderExitCallback,
    /// Checked on every loop iteration, including the idle poll that fires
    /// at least once a second, so a core poisoned off the reader thread ends
    /// the pane even when the child prints nothing. Without it only the next
    /// read would notice (`PtyReadResult::CoreBroken`), and an idle pane
    /// would stay frozen until another read arrived.
    pub core_broken: CoreBrokenCheck,
}

#[derive(Clone)]
pub struct PtyIoActorHandle {
    pane_id: PaneId,
    wake: fd::WakeWriter,
    inbox: Arc<Mutex<PtyIoInbox>>,
    /// Lock order for terminal mutations that can produce replies is
    /// `response_order` > content-write lock > terminal core. The actor holds
    /// this across parsing a read and queuing its replies; reply producers
    /// hold it across their closure and queuing. Read-side effects run only
    /// after it is released. It is separate from the inbox lock so user input
    /// is never queued behind a parse or a wait for the terminal core.
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
    next_entry_id: u64,
    latest_resize: Option<QueuedResize>,
    terminal_response_drops: u64,
    terminal_response_drop_reported_count: u64,
    shutdown: bool,
}

struct PtyIoInboxEntry {
    id: u64,
    order: u64,
    write: PendingWrite,
}

struct QueuedResize {
    order: u64,
    resize: PtyResize,
    terminal_responses: Vec<Bytes>,
}

/// What the inbox did with one terminal reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponsePush {
    /// The reply is queued (an empty reply needs no queueing and counts too).
    Queued,
    /// The inbox was full, so the reply was dropped. `first` marks the first
    /// drop since the inbox began, the only one reported as it happens.
    Dropped { first: bool },
}

impl PtyIoInbox {
    fn next_order(&mut self) -> u64 {
        let order = self.next_order;
        self.next_order = self.next_order.wrapping_add(1);
        order
    }

    fn next_entry_id(&mut self) -> u64 {
        let id = self.next_entry_id;
        self.next_entry_id = self.next_entry_id.wrapping_add(1);
        id
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
        let id = self.next_entry_id();
        self.entries.push_back(PtyIoInboxEntry {
            id,
            order,
            write: PendingWrite::User(bytes),
        });
        Ok(())
    }

    fn push_terminal_response(&mut self, bytes: Bytes) -> ResponsePush {
        if bytes.is_empty() {
            return ResponsePush::Queued;
        }
        // Only a child that has stopped reading fills the inbox, and a reply
        // it will read late is worth little, so an overflowing reply is
        // dropped; user input instead gets Full. The replies already queued
        // keep their order, and a later reply that fits still goes out (a
        // DA1 sentinel behind a dropped answer tells the child the answer is
        // not coming rather than leaving it waiting).
        if !self.reserve(bytes.len(), 1) {
            return ResponsePush::Dropped {
                first: self.note_terminal_response_drop(),
            };
        }
        let order = self.next_order();
        let id = self.next_entry_id();
        self.entries.push_back(PtyIoInboxEntry {
            id,
            order,
            write: PendingWrite::TerminalResponse(bytes),
        });
        ResponsePush::Queued
    }

    fn note_terminal_response_drop(&mut self) -> bool {
        self.terminal_response_drops = self.terminal_response_drops.saturating_add(1);
        // Only the first drop is reported as it happens; later drops wait for
        // the actor's shutdown total.
        self.terminal_response_drops == 1
    }

    fn mark_terminal_response_drops_reported(&mut self) -> Option<u64> {
        if self.terminal_response_drops <= self.terminal_response_drop_reported_count {
            return None;
        }
        self.terminal_response_drop_reported_count = self.terminal_response_drops;
        Some(self.terminal_response_drops)
    }

    /// Coalesce resizes: only the newest geometry matters to the PTY, and the
    /// replies of a superseded request describe a size that no longer holds.
    fn replace_resize(
        &mut self,
        geometry: shepr_core::geometry::PaneGeometry,
        terminal_responses: Vec<Bytes>,
    ) -> Option<u64> {
        if let Some(previous) = self.latest_resize.take() {
            for bytes in previous.terminal_responses {
                self.release_bytes(bytes.len());
                self.release_item();
            }
        }

        let mut accepted_responses = Vec::new();
        let mut should_report_drop = false;
        for bytes in terminal_responses {
            if bytes.is_empty() {
                continue;
            }
            if self.reserve(bytes.len(), 1) {
                accepted_responses.push(bytes);
            } else if self.note_terminal_response_drop() {
                should_report_drop = true;
            }
        }
        let order = self.next_order();
        self.latest_resize = Some(QueuedResize {
            order,
            resize: PtyResize { geometry },
            terminal_responses: accepted_responses,
        });
        if should_report_drop {
            self.mark_terminal_response_drops_reported()
        } else {
            None
        }
    }

    /// Whether a resize not yet applied holds its replies ahead of an entry.
    /// Everything queued after the resize waits until the actor applies it
    /// (on its next inbox pass), so no later reply overtakes its replies.
    fn resize_holds(&self, order: u64) -> bool {
        self.latest_resize
            .as_ref()
            .is_some_and(|resize| !resize.terminal_responses.is_empty() && resize.order < order)
    }

    /// The write the actor can start or continue now, if any. A write already
    /// under way always continues; otherwise it is the front of the queue.
    fn writable_index(&self, current_write_id: Option<u64>) -> Option<usize> {
        if let Some(id) = current_write_id {
            // A write under way was started before any resize that could hold it.
            return self.entries.iter().position(|entry| entry.id == id);
        }
        let entry = self.entries.front()?;
        if self.resize_holds(entry.order) {
            return None;
        }
        Some(0)
    }

    /// Remove an entry and release what it reserved. `written` bytes of it
    /// were already released as they were written.
    fn remove_entry(&mut self, index: usize, written: usize) -> Option<PendingWrite> {
        let entry = self.entries.remove(index)?;
        let (PendingWrite::User(bytes) | PendingWrite::TerminalResponse(bytes)) = &entry.write;
        self.release_bytes(bytes.len().saturating_sub(written));
        self.release_item();
        Some(entry.write)
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
            let id = self.next_entry_id();
            self.entries.insert(
                insert_at + offset,
                PtyIoInboxEntry {
                    id,
                    order,
                    write: PendingWrite::TerminalResponse(bytes),
                },
            );
        }
    }
}

impl PtyIoActorHandle {
    pub fn try_write_user_input(&self, bytes: Bytes) -> Result<(), ChildIoSendError> {
        let result = {
            let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
            if inbox.shutdown {
                return Err(ChildIoSendError::Closed(bytes));
            }
            inbox.push_user_input(bytes)
        };
        match result {
            Ok(()) => {
                self.wake_actor();
                Ok(())
            }
            Err(bytes) => Err(ChildIoSendError::Full(bytes)),
        }
    }

    /// Queue a terminal reply produced outside a PTY read. `response` runs
    /// under the reply-order lock (it may take the terminal core lock), never
    /// under the inbox lock.
    pub fn write_terminal_response(&self, response: impl FnOnce() -> Option<Bytes>) {
        self.write_terminal_responses(|| response().into_iter().collect());
    }

    /// Produce and queue every reply from one terminal operation at one point
    /// in the response order. The closure may take the terminal core lock,
    /// but runs without the inbox lock.
    pub fn write_terminal_responses(&self, responses: impl FnOnce() -> Vec<Bytes>) {
        let order = crate::locks::lock_auxiliary(&self.response_order);
        let responses = responses();
        let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
        if inbox.shutdown {
            return;
        }
        let mut queued = false;
        let mut should_report_drop = false;
        for response in responses {
            match inbox.push_terminal_response(response) {
                ResponsePush::Queued => queued = true,
                ResponsePush::Dropped { first } => should_report_drop |= first,
            }
        }
        let reported_drop_count = if should_report_drop {
            inbox.mark_terminal_response_drops_reported()
        } else {
            None
        };
        drop(inbox);
        drop(order);
        if let Some(dropped_responses) = reported_drop_count {
            report_terminal_response_drops(self.pane_id, dropped_responses);
        }
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
        let reported_drop_count = inbox.replace_resize(geometry, terminal_responses);
        drop(inbox);
        drop(_order);
        if let Some(dropped_responses) = reported_drop_count {
            report_terminal_response_drops(self.pane_id, dropped_responses);
        }
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
            debug!(error = %err, "failed to wake PTY actor");
        }
    }
}

fn report_terminal_response_drops(pane_id: PaneId, dropped_responses: u64) {
    warn!(
        pane = %pane_id,
        dropped_responses, "PTY terminal reply inbox is full; dropped terminal replies"
    );
}

fn report_terminal_response_drop_total(pane_id: PaneId, total_dropped_responses: u64) {
    warn!(
        pane = %pane_id,
        total_dropped_responses, "PTY actor stopped after dropping terminal replies"
    );
}

pub struct PtyIoActor;

impl PtyIoActor {
    pub fn spawn(config: PtyIoActorConfig) -> std::io::Result<PtyIoActorHandle> {
        Self::spawn_inner(config, SystemPtyIo)
    }

    fn spawn_inner<I: PtyIo + Send + 'static>(
        config: PtyIoActorConfig,
        io: I,
    ) -> std::io::Result<PtyIoActorHandle> {
        // Production masters come from open_pty_with_geometry with
        // O_CLOEXEC set atomically. The injected actor seam may use other fds
        // in tests, but it does not own their child-inheritance policy.
        fd::set_nonblocking(config.master_fd.as_raw_fd())?;

        let wake_pipe = fd::create_wake_pipe()?;
        let inbox = Arc::new(Mutex::new(PtyIoInbox::default()));
        let response_order = Arc::new(Mutex::new(()));
        let handle = PtyIoActorHandle {
            pane_id: config.pane_id,
            wake: wake_pipe.writer,
            inbox: Arc::clone(&inbox),
            response_order: Arc::clone(&response_order),
        };

        let runner = PtyIoActorRunner {
            pane_id: config.pane_id,
            file: std::fs::File::from(config.master_fd),
            inbox,
            response_order,
            current_write_id: None,
            current_write_offset: 0,
            wake_read_fd: wake_pipe.read_fd,
            on_read: config.on_read,
            on_reader_exit: Some(config.on_reader_exit),
            core_broken: config.core_broken,
            exit_reason: ReaderExit::ShutdownRequested,
            io,
            resize_failure_logged: false,
        };
        std::thread::Builder::new()
            .name(format!("shepr-pty-{}", config.pane_id))
            .spawn(move || runner.run())?;

        Ok(handle)
    }
}

trait PtyIo {
    fn poll(
        &mut self,
        pty: RawFd,
        wake: RawFd,
        writable: bool,
        wait: Wait,
    ) -> std::io::Result<fd::PtyWakeReadiness>;
    fn drain(&mut self, wake: RawFd) -> std::io::Result<()>;
    fn resize(&mut self, pty: RawFd, resize: PtyResize) -> std::io::Result<()>;
}

struct SystemPtyIo;

impl PtyIo for SystemPtyIo {
    fn poll(
        &mut self,
        pty: RawFd,
        wake: RawFd,
        writable: bool,
        wait: Wait,
    ) -> std::io::Result<fd::PtyWakeReadiness> {
        fd::poll_pty_and_wake(pty, wake, writable, wait)
    }

    fn drain(&mut self, wake: RawFd) -> std::io::Result<()> {
        fd::drain_wake_fd(wake)
    }

    fn resize(&mut self, pty: RawFd, resize: PtyResize) -> std::io::Result<()> {
        resize_pty(pty, resize)
    }
}

struct PtyIoActorRunner<I: PtyIo> {
    pane_id: PaneId,
    file: std::fs::File,
    inbox: Arc<Mutex<PtyIoInbox>>,
    response_order: Arc<Mutex<()>>,
    /// The entry whose write is under way, and how much of it is written.
    /// The offset is zero whenever the entry id is `None`.
    current_write_id: Option<u64>,
    current_write_offset: usize,
    wake_read_fd: OwnedFd,
    on_read: ReadCallback,
    on_reader_exit: Option<ReaderExitCallback>,
    core_broken: CoreBrokenCheck,
    /// Only raised, through `raise_exit`. It starts at `ShutdownRequested`,
    /// which is what a loop that leaves on the shutdown flag reports; every
    /// other way out raises it first.
    exit_reason: ReaderExit,
    io: I,
    /// Whether a resize failure was already reported at warn level.
    resize_failure_logged: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum WriteStep {
    /// Nothing can be written now.
    Idle,
    /// The PTY would block.
    Blocked,
    /// Bytes were written or an entry was retired.
    Progress,
}

#[derive(Debug, PartialEq, Eq)]
enum PendingWrite {
    User(Bytes),
    TerminalResponse(Bytes),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadOutcome {
    Data,
    WouldBlock,
    Interrupted,
    /// The loop must end; `exit_reason` says why.
    Closed,
}

fn pty_master_error_means_child_closed(error: &std::io::Error) -> bool {
    // Linux reports EIO on the master after the child closes the slave.
    error.raw_os_error() == Some(libc::EIO)
}

/// How a hard PTY read or write error ends the loop.
fn exit_for_pty_error(error: &std::io::Error) -> ReaderExit {
    if pty_master_error_means_child_closed(error) {
        ReaderExit::Closed
    } else {
        ReaderExit::IoFailed
    }
}

impl<I: PtyIo> PtyIoActorRunner<I> {
    fn raise_exit(&mut self, exit: ReaderExit) {
        if exit.severity() > self.exit_reason.severity() {
            self.exit_reason = exit;
        }
    }

    /// Runs the IO loop, then closes the master and the wake pipe before
    /// reporting the exit, so whatever the owner does next (waiting for the
    /// child to react to the hangup) happens with the PTY already closed.
    fn run(mut self) {
        self.run_loop();
        let exit = self.exit_reason;
        let on_reader_exit = self.on_reader_exit.take();
        let pane_id = self.pane_id;
        drop(self);
        if let Some(on_reader_exit) = on_reader_exit {
            on_reader_exit(exit);
        }
        debug!(pane = %pane_id, "PTY actor exiting");
    }

    fn run_loop(&mut self) {
        loop {
            if crate::locks::lock_auxiliary(&self.inbox).shutdown {
                break;
            }
            if (self.core_broken)() {
                error!(
                    pane = %self.pane_id,
                    "terminal core is broken by a panic elsewhere; closing the pane"
                );
                self.raise_exit(ReaderExit::Panicked);
                break;
            }

            // The wake pipe is drained right after poll returns, before this
            // pump reads the inbox, so work pushed after the pump has read it
            // always leaves a wake byte for the poll below.
            if let Err(err) = self.pump() {
                self.handle_write_failure(&err);
                break;
            }
            // The poll helper retries EINTR. A hard poll or wake-drain error
            // stops reads while the child may still be alive, so report an
            // IO failure that makes the mux remove this pane and tear it down.
            let writable = self.has_writable_work();
            match self.io.poll(
                self.file.as_raw_fd(),
                self.wake_read_fd.as_raw_fd(),
                writable,
                Wait::After(ACTOR_IDLE_POLL),
            ) {
                Ok(readiness) => {
                    if readiness.wake_ready
                        && let Err(err) = self.io.drain(self.wake_read_fd.as_raw_fd())
                    {
                        error!(
                            pane = %self.pane_id,
                            error = %err,
                            "PTY actor wake drain failed; closing the pane"
                        );
                        self.raise_exit(ReaderExit::IoFailed);
                        break;
                    }
                    // POLLERR is part of read readiness: the read classifies
                    // it like any other ending. An error with nothing to read
                    // would report again on every poll, so it ends the loop.
                    if readiness.pty_read_ready {
                        match self.read_chunk() {
                            ReadOutcome::Closed => break,
                            ReadOutcome::WouldBlock if readiness.pty_error => {
                                error!(
                                    pane = %self.pane_id,
                                    "PTY reported an error with nothing to read; closing the pane"
                                );
                                self.raise_exit(ReaderExit::IoFailed);
                                break;
                            }
                            ReadOutcome::Data
                            | ReadOutcome::WouldBlock
                            | ReadOutcome::Interrupted => {}
                        }
                    }
                    if readiness.pty_write_ready
                        && let Err(err) = self.pump()
                    {
                        self.handle_write_failure(&err);
                        break;
                    }
                }
                Err(err) => {
                    error!(
                        pane = %self.pane_id,
                        error = %err,
                        "PTY actor poll failed; closing the pane"
                    );
                    self.raise_exit(ReaderExit::IoFailed);
                    break;
                }
            }
        }

        if let Some(total_dropped_responses) = self.close_inbox() {
            report_terminal_response_drop_total(self.pane_id, total_dropped_responses);
        }
    }

    /// Write what the PTY takes now, applying any pending resize around each
    /// write, so nothing that is ready waits for the next wake or the idle
    /// poll.
    fn pump(&mut self) -> std::io::Result<()> {
        for _ in 0..MAX_WRITE_STEPS_PER_PUMP {
            self.apply_pending_resizes();
            match self.write_next()? {
                WriteStep::Idle | WriteStep::Blocked => return Ok(()),
                WriteStep::Progress => {}
            }
        }
        // Out of steps with the PTY still writable: the next poll returns at
        // once. Apply a pending resize first so it is not left waiting.
        self.apply_pending_resizes();
        Ok(())
    }

    /// Apply resizes until none is pending. Each pass either applies one or
    /// finds a newer request replaced it during the ioctl, so this ends.
    fn apply_pending_resizes(&mut self) {
        while self.apply_pending_resize() {}
    }

    /// Apply the pending resize. The ioctl runs as soon as the request
    /// arrives, not behind queued writes: a child that is not reading stdin
    /// must still get its SIGWINCH. Only the replies take the request's place
    /// in the sequence. Returns whether anything changed.
    ///
    /// A failed ioctl is logged and not retried. TIOCSWINSZ on a PTY master
    /// this actor owns has no transient failure: Linux resizes the pair under
    /// a plain mutex and fails only on a bad fd, a bad pointer or a non-tty,
    /// none of which a later attempt changes. The replies go out either way:
    /// they describe the emulator, which the runtime has already resized.
    fn apply_pending_resize(&mut self) -> bool {
        let (order, resize) = {
            let inbox = crate::locks::lock_auxiliary(&self.inbox);
            let Some(pending) = inbox.latest_resize.as_ref() else {
                return false;
            };
            (pending.order, pending.resize)
        };
        let result = self.io.resize(self.file.as_raw_fd(), resize);

        let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
        if inbox
            .latest_resize
            .as_ref()
            .is_none_or(|pending| pending.order != order)
        {
            // Replaced during the ioctl; the newer request is applied next.
            return true;
        }
        if let Some(done) = inbox.latest_resize.take() {
            inbox.insert_resize_replies(done.order, done.terminal_responses);
        }
        drop(inbox);
        if let Err(err) = result {
            // A failure that cannot clear would repeat on every resize, so
            // only the first one per pane is a warning.
            if self.resize_failure_logged {
                debug!(pane = %self.pane_id, error = %err, "PTY resize failed");
            } else {
                self.resize_failure_logged = true;
                warn!(
                    pane = %self.pane_id,
                    error = %err,
                    "PTY resize failed; the child keeps its previous window size"
                );
            }
        }
        true
    }

    /// A write failure usually means the child has gone (the master reports EIO
    /// once the slave side is closed), but whatever it printed before exiting is
    /// still buffered on the master. Read that out before the loop ends so the
    /// child's last output reaches the terminal. Bounded so a peer that keeps
    /// producing output cannot hold the actor here.
    fn handle_write_failure(&mut self, err: &std::io::Error) {
        debug!(pane = %self.pane_id, error = %err, "PTY actor stopping after a write failure");
        // Raised first, so the drain's own ending cannot lower it.
        self.raise_exit(exit_for_pty_error(err));
        for _ in 0..MAX_WRITE_FAILURE_DRAIN_CHUNKS {
            match self.read_chunk() {
                ReadOutcome::Data | ReadOutcome::Interrupted => {}
                ReadOutcome::WouldBlock | ReadOutcome::Closed => break,
            }
        }
        // Replies generated while draining are discarded when the inbox closes.
        self.current_write_id = None;
        self.current_write_offset = 0;
    }

    fn read_chunk(&mut self) -> ReadOutcome {
        let mut buf = [0u8; PTY_READ_BUFFER_BYTES];
        match self.file.read(&mut buf) {
            Ok(0) => {
                self.raise_exit(ReaderExit::Closed);
                ReadOutcome::Closed
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => ReadOutcome::WouldBlock,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => ReadOutcome::Interrupted,
            Err(err) => {
                let exit = exit_for_pty_error(&err);
                if exit == ReaderExit::Closed {
                    debug!(
                        pane = %self.pane_id,
                        error = %err,
                        "PTY actor read ended after the child closed its terminal"
                    );
                } else {
                    error!(
                        pane = %self.pane_id,
                        error = %err,
                        "PTY actor read failed; closing the pane"
                    );
                }
                self.raise_exit(exit);
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
                                pane = %self.pane_id,
                                panic = shepr_core::panic_message(
                                    &*payload,
                                    "non-string panic payload"
                                ),
                                "PTY read callback panicked; closing the pane"
                            );
                            self.raise_exit(ReaderExit::Panicked);
                            return ReadOutcome::Closed;
                        }
                    };
                let effects = match result {
                    PtyReadResult::Effects(effects) => effects,
                    PtyReadResult::CoreBroken => {
                        error!(
                            pane = %self.pane_id,
                            "terminal core is broken by an earlier panic; closing the pane"
                        );
                        self.raise_exit(ReaderExit::Panicked);
                        return ReadOutcome::Closed;
                    }
                };
                let PtyReadEffects {
                    terminal_responses,
                    after_response_order,
                } = effects;
                let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
                let mut should_report_drop = false;
                if !inbox.shutdown {
                    for response in terminal_responses {
                        if let ResponsePush::Dropped { first } =
                            inbox.push_terminal_response(response)
                        {
                            should_report_drop |= first;
                        }
                    }
                }
                let reported_drop_count = if should_report_drop {
                    inbox.mark_terminal_response_drops_reported()
                } else {
                    None
                };
                drop(inbox);
                drop(_order);
                if let Some(dropped_responses) = reported_drop_count {
                    report_terminal_response_drops(self.pane_id, dropped_responses);
                }
                if let Some(after_response_order) = after_response_order {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "post-read effects must not unwind out of the actor thread; a panic is reported like a broken read callback"
                    )]
                    let effects_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        after_response_order,
                    ));
                    if let Err(payload) = effects_result {
                        error!(
                            pane = %self.pane_id,
                            panic =
                                shepr_core::panic_message(&*payload, "non-string panic payload"),
                            "PTY post-read effects panicked; closing the pane"
                        );
                        self.raise_exit(ReaderExit::Panicked);
                        return ReadOutcome::Closed;
                    }
                }
                ReadOutcome::Data
            }
        }
    }

    fn close_inbox(&mut self) -> Option<u64> {
        let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
        inbox.shutdown = true;
        inbox.entries.clear();
        inbox.latest_resize = None;
        inbox.pending_bytes = 0;
        inbox.pending_items = 0;
        // The first overflow is reported immediately; include later drops
        // in one final total when the actor stops.
        inbox.mark_terminal_response_drops_reported()
    }

    /// Take one write step on the next writable entry. The inbox lock is
    /// released around the write syscall; the entry's index stays valid
    /// because only this thread removes or inserts entries.
    fn write_next(&mut self) -> std::io::Result<WriteStep> {
        let (index, id, bytes) = {
            let inbox = crate::locks::lock_auxiliary(&self.inbox);
            let Some(index) = inbox.writable_index(self.current_write_id) else {
                return Ok(WriteStep::Idle);
            };
            let Some(entry) = inbox.entries.get(index) else {
                return Ok(WriteStep::Idle);
            };
            let (PendingWrite::User(bytes) | PendingWrite::TerminalResponse(bytes)) = &entry.write;
            (index, entry.id, bytes.clone())
        };
        let offset = self.current_write_offset;

        let write_result = self.file.write(&bytes[offset..]);
        match write_result {
            Ok(0) => Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "PTY actor write returned zero bytes",
            )),
            Ok(written) => {
                let offset = offset.saturating_add(written);
                if offset < bytes.len() {
                    crate::locks::lock_auxiliary(&self.inbox).release_bytes(written);
                    self.current_write_id = Some(id);
                    self.current_write_offset = offset;
                    return Ok(WriteStep::Progress);
                }
                // `retire_entry` releases what the earlier chunks left.
                self.retire_entry(index, id, offset.saturating_sub(written));
                Ok(WriteStep::Progress)
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(WriteStep::Blocked),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => Ok(WriteStep::Progress),
            Err(err) => {
                if pty_master_error_means_child_closed(&err) {
                    debug!(
                        pane = %self.pane_id,
                        error = %err,
                        "PTY actor write ended after the child closed its terminal"
                    );
                } else {
                    error!(
                        pane = %self.pane_id,
                        error = %err,
                        "PTY actor write failed; closing the pane"
                    );
                }
                Err(err)
            }
        }
    }

    /// Remove a finished entry. `released` bytes of it were released as
    /// earlier chunks were written.
    fn retire_entry(&mut self, index: usize, id: u64, released: usize) {
        self.current_write_id = None;
        self.current_write_offset = 0;
        let mut inbox = crate::locks::lock_auxiliary(&self.inbox);
        if inbox.entries.get(index).is_some_and(|entry| entry.id == id) {
            inbox.remove_entry(index, released);
        }
    }

    fn has_writable_work(&self) -> bool {
        crate::locks::lock_auxiliary(&self.inbox)
            .writable_index(self.current_write_id)
            .is_some()
    }
}

fn resize_pty(fd: RawFd, resize: PtyResize) -> std::io::Result<()> {
    fd::resize_pty_fd(fd, resize.geometry)
}

#[cfg(test)]
impl PtyReadResult {
    fn empty() -> Self {
        Self::Effects(PtyReadEffects {
            terminal_responses: Vec::new(),
            after_response_order: None,
        })
    }
}

#[cfg(test)]
use std::sync::mpsc as std_mpsc;

#[cfg(test)]
struct TestPtyIo {
    poll_observer: Option<std_mpsc::Sender<()>>,
    resize_pty: Box<dyn FnMut(RawFd, PtyResize) -> std::io::Result<()> + Send>,
    poll_pty_and_wake: fn(RawFd, RawFd, bool, Wait) -> std::io::Result<fd::PtyWakeReadiness>,
    drain_wake_fd: fn(RawFd) -> std::io::Result<()>,
}

#[cfg(test)]
impl Default for TestPtyIo {
    fn default() -> Self {
        Self {
            poll_observer: None,
            resize_pty: Box::new(resize_pty),
            poll_pty_and_wake: fd::poll_pty_and_wake,
            drain_wake_fd: fd::drain_wake_fd,
        }
    }
}

#[cfg(test)]
impl PtyIo for TestPtyIo {
    fn poll(
        &mut self,
        pty: RawFd,
        wake: RawFd,
        writable: bool,
        wait: Wait,
    ) -> std::io::Result<fd::PtyWakeReadiness> {
        if let Some(observer) = &self.poll_observer {
            observer.send(()).ok();
        }
        (self.poll_pty_and_wake)(pty, wake, writable, wait)
    }

    fn drain(&mut self, wake: RawFd) -> std::io::Result<()> {
        (self.drain_wake_fd)(wake)
    }

    fn resize(&mut self, pty: RawFd, resize: PtyResize) -> std::io::Result<()> {
        (self.resize_pty)(pty, resize)
    }
}

#[cfg(test)]
impl PtyIoActor {
    fn spawn_with_poll_observer(
        config: PtyIoActorConfig,
        poll_observer: std_mpsc::Sender<()>,
    ) -> std::io::Result<PtyIoActorHandle> {
        Self::spawn_inner(
            config,
            TestPtyIo {
                poll_observer: Some(poll_observer),
                ..TestPtyIo::default()
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::fd::{AsRawFd, FromRawFd, IntoRawFd},
        os::unix::net::UnixStream,
        sync::atomic::{AtomicBool, Ordering},
        time::{Duration, Instant},
    };

    fn test_pane_id() -> PaneId {
        PaneId::from_raw(1)
    }

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
            pane_id: test_pane_id(),
            master_fd: owned,
            on_read: Box::new(move |bytes| {
                read_tx
                    .send(Bytes::copy_from_slice(bytes))
                    .expect("read callback receiver alive");
                PtyReadResult::empty()
            }),
            on_reader_exit: Box::new(|_| {}),
            core_broken: Box::new(|| false),
        };
        let handle = if let Some(poll_observer) = poll_observer {
            PtyIoActor::spawn_with_poll_observer(config, poll_observer)
        } else {
            PtyIoActor::spawn(config)
        }
        .expect("actor spawn");
        (handle, peer, read_rx)
    }

    fn actor_test_parts(
        on_read: ReadCallback,
    ) -> (PtyIoActorRunner<TestPtyIo>, PtyIoActorHandle, UnixStream) {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        // SAFETY: into_raw_fd transfers this socket's sole fd ownership to OwnedFd.
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        let inbox = Arc::new(Mutex::new(PtyIoInbox::default()));
        let response_order = Arc::new(Mutex::new(()));
        let pane_id = test_pane_id();
        let handle = PtyIoActorHandle {
            pane_id,
            wake: wake_pipe.writer,
            inbox: Arc::clone(&inbox),
            response_order: Arc::clone(&response_order),
        };
        let runner = PtyIoActorRunner {
            pane_id,
            file: std::fs::File::from(owned),
            inbox,
            response_order,
            current_write_id: None,
            current_write_offset: 0,
            wake_read_fd: wake_pipe.read_fd,
            on_read,
            on_reader_exit: None,
            core_broken: Box::new(|| false),
            exit_reason: ReaderExit::ShutdownRequested,
            io: TestPtyIo::default(),
            resize_failure_logged: false,
        };
        (runner, handle, peer)
    }

    fn actor_runner_for_unit_test() -> (PtyIoActorRunner<TestPtyIo>, UnixStream) {
        let (runner, _handle, peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        (runner, peer)
    }

    #[test]
    fn write_failure_still_delivers_the_childs_last_output() {
        let (read_tx, read_rx) = std_mpsc::channel();
        let (runner, _handle, mut peer) = actor_test_parts(Box::new(move |bytes| {
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
            pane_id: test_pane_id(),
            wake,
            inbox: Arc::new(Mutex::new(PtyIoInbox::default())),
            response_order: Arc::new(Mutex::new(())),
        };
        handle
            .try_write_user_input(Bytes::from(vec![b'f'; ACTOR_INBOX_MAX_BYTES]))
            .expect("first write fits the queue");
        match handle.try_write_user_input(Bytes::from_static(b"full")) {
            Err(ChildIoSendError::Full(bytes)) => assert_eq!(bytes, "full"),
            other => panic!("expected a full queue, got {other:?}"),
        }
        handle.shutdown();
        match handle.try_write_user_input(Bytes::from_static(b"closed")) {
            Err(ChildIoSendError::Closed(bytes)) => assert_eq!(bytes, "closed"),
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
            pane_id: test_pane_id(),
            master_fd: owned,
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: Box::new(|_| {}),
            core_broken: Box::new(|| false),
        })
        .expect("actor spawn");

        let chunk = Bytes::from(vec![b'x'; 16 * 1024]);
        let mut rejected = None;
        for _ in 0..(ACTOR_INBOX_MAX_BYTES / chunk.len() + 8) {
            match handle.try_write_user_input(chunk.clone()) {
                Ok(()) => {}
                Err(ChildIoSendError::Full(bytes)) => {
                    rejected = Some(bytes);
                    break;
                }
                Err(ChildIoSendError::Closed(_)) => panic!("live actor closed unexpectedly"),
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
    fn actor_rejects_user_input_after_io_loop_exits() {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        // SAFETY: into_raw_fd transfers this socket's sole fd ownership to OwnedFd.
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let handle_slot = Arc::new(Mutex::new(None::<PtyIoActorHandle>));
        let (attempt_tx, attempt_rx) = std_mpsc::channel();
        let config = PtyIoActorConfig {
            pane_id: test_pane_id(),
            master_fd: owned,
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: Box::new({
                let handle_slot = Arc::clone(&handle_slot);
                move |_| {
                    let handle = crate::locks::lock_auxiliary(&handle_slot)
                        .as_ref()
                        .expect("actor handle installed")
                        .clone();
                    let attempt = handle.try_write_user_input(Bytes::from_static(b"late"));
                    attempt_tx.send(attempt).expect("attempt receiver alive");
                }
            }),
            core_broken: Box::new(|| false),
        };
        let handle = PtyIoActor::spawn(config).expect("actor spawn");
        *crate::locks::lock_auxiliary(&handle_slot) = Some(handle);

        drop(peer);
        match attempt_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("reader exit callback attempts a write")
        {
            Err(ChildIoSendError::Closed(bytes)) => assert_eq!(bytes, "late"),
            other => panic!("expected a closed queue, got {other:?}"),
        }
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
            pane_id: test_pane_id(),
            master_fd: owned,
            on_read,
            on_reader_exit: Box::new(move |exit| {
                // The actor thread can outlive a test that already finished
                // and dropped the receiver; tests that check the exit hold it.
                exit_tx.send(exit).ok();
            }),
            core_broken: core_broken.unwrap_or_else(|| Box::new(|| false)),
        })
        .expect("actor spawn");
        (handle, peer, exit_rx)
    }

    fn wait_readable(fd: RawFd) -> std::io::Result<()> {
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: `poll_fd` is one live pollfd and poll does not retain it.
            let result = unsafe { libc::poll(&mut poll_fd, 1, 1000) };
            if result > 0 {
                return if poll_fd.revents & libc::POLLIN != 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::other(
                        "slave PTY became ready without readable input",
                    ))
                };
            }
            if result == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "slave PTY did not receive the actor response",
                ));
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
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
        let (_handle, mut peer, exit_rx) =
            actor_reporting_exit(Box::new(|_| PtyReadResult::CoreBroken));

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

    /// A write failure that is not the PTY's child-closed EIO (EPIPE on this
    /// socket stand-in) stays an IO failure when the drain then reads EOF.
    #[test]
    fn a_hard_write_failure_is_not_lowered_by_the_drain() {
        let (mut runner, _handle, mut peer) =
            actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        let (exit_tx, exit_rx) = std_mpsc::channel();
        runner.on_reader_exit = Some(Box::new(move |reason| {
            exit_tx
                .send(reason)
                .expect("reader exit receiver stays alive");
        }));
        peer.write_all(b"last-output").expect("peer write");
        drop(peer);
        crate::locks::lock_auxiliary(&runner.inbox)
            .push_user_input(Bytes::from_static(b"queued-reply"))
            .expect("test write fits inbox");

        runner.run();

        assert_eq!(
            exit_rx.try_recv().expect("reader exit is reported"),
            ReaderExit::IoFailed
        );
    }

    #[test]
    fn a_pty_error_with_nothing_to_read_reports_io_failed() {
        let (mut runner, _handle, _peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        let (exit_tx, exit_rx) = std_mpsc::channel();
        runner.on_reader_exit = Some(Box::new(move |reason| {
            exit_tx
                .send(reason)
                .expect("reader exit receiver stays alive");
        }));
        runner.io.poll_pty_and_wake = |_, _, _, _| {
            Ok(fd::PtyWakeReadiness {
                pty_read_ready: true,
                pty_error: true,
                ..Default::default()
            })
        };

        runner.run();

        assert_eq!(
            exit_rx.try_recv().expect("reader exit is reported"),
            ReaderExit::IoFailed
        );
    }

    #[test]
    fn a_requested_shutdown_is_reported_as_one() {
        let (mut runner, handle, _peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        let (exit_tx, exit_rx) = std_mpsc::channel();
        runner.on_reader_exit = Some(Box::new(move |reason| {
            exit_tx
                .send(reason)
                .expect("reader exit receiver stays alive");
        }));
        handle.shutdown();

        runner.run();

        assert_eq!(
            exit_rx.try_recv().expect("reader exit is reported"),
            ReaderExit::ShutdownRequested
        );
    }

    #[test]
    fn hard_poll_failure_reports_io_failed() {
        let (mut runner, _handle, _peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        let (exit_tx, exit_rx) = std_mpsc::channel();
        runner.on_reader_exit = Some(Box::new(move |reason| {
            exit_tx
                .send(reason)
                .expect("reader exit receiver stays alive");
        }));
        runner.io.poll_pty_and_wake =
            |_, _, _, _| Err(std::io::Error::other("injected poll failure"));

        runner.run();

        assert_eq!(
            exit_rx.try_recv().expect("reader exit is reported"),
            ReaderExit::IoFailed
        );
    }

    #[test]
    fn wake_drain_failure_reports_io_failed() {
        let (mut runner, _handle, _peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        let (exit_tx, exit_rx) = std_mpsc::channel();
        runner.on_reader_exit = Some(Box::new(move |reason| {
            exit_tx
                .send(reason)
                .expect("reader exit receiver stays alive");
        }));
        runner.io.poll_pty_and_wake = |_, _, _, _| {
            Ok(fd::PtyWakeReadiness {
                wake_ready: true,
                ..Default::default()
            })
        };
        runner.io.drain_wake_fd = |_| Err(std::io::Error::other("injected wake drain failure"));

        runner.run();

        assert_eq!(
            exit_rx.try_recv().expect("reader exit is reported"),
            ReaderExit::IoFailed
        );
    }

    #[test]
    fn actor_open_pty_handles_io_resize_and_slave_close() {
        let crate::backend::OpenedPty { master, slave } = crate::backend::open_pty_with_geometry(
            shepr_core::geometry::PaneGeometry::cells_only(80, 24),
        )
        .expect("open PTY pair");
        let control_master = master.try_clone().expect("clone PTY master for ioctl");
        let mut slave = std::fs::File::from(slave);
        let (read_tx, read_rx) = std_mpsc::channel::<Bytes>();
        let (exit_tx, exit_rx) = std_mpsc::channel();
        let handle = PtyIoActor::spawn(PtyIoActorConfig {
            pane_id: test_pane_id(),
            master_fd: master,
            on_read: Box::new(move |bytes| {
                read_tx
                    .send(Bytes::copy_from_slice(bytes))
                    .expect("read receiver stays alive through actor exit");
                PtyReadResult::empty()
            }),
            on_reader_exit: Box::new(move |reason| {
                exit_tx.send(reason).expect("exit receiver stays alive");
            }),
            core_broken: Box::new(|| false),
        })
        .expect("start actor on PTY master");

        slave.write_all(b"pty-output").expect("write slave output");
        let mut output = Vec::new();
        while output.len() < b"pty-output".len() {
            let chunk = read_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("actor reads slave output");
            output.extend_from_slice(&chunk);
        }
        assert_eq!(output, b"pty-output");

        handle.resize(
            shepr_core::geometry::PaneGeometry::with_cell(
                100,
                40,
                shepr_core::geometry::CellPx::new(1_000, 20),
            ),
            || vec![Bytes::from_static(b"resize-ok\n")],
        );
        wait_readable(slave.as_raw_fd()).expect("actor applies resize and writes its reply");
        let mut reply = [0; b"resize-ok\n".len()];
        slave
            .read_exact(&mut reply)
            .expect("read actor resize reply");
        assert_eq!(&reply, b"resize-ok\n");

        // SAFETY: zero is a valid initial byte representation for winsize, and
        // TIOCGWINSZ writes one winsize to this live local value.
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: the cloned master fd is live, and ioctl writes only `size`.
        let result =
            unsafe { libc::ioctl(control_master.as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
        assert_eq!(result, 0, "TIOCGWINSZ succeeds");
        assert_eq!(
            (size.ws_row, size.ws_col, size.ws_xpixel, size.ws_ypixel),
            (40, 100, u16::MAX, 800)
        );

        drop(slave);
        assert_eq!(
            exit_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("slave closure wakes actor through PTY hangup"),
            ReaderExit::Closed
        );

        let mut master_probe = std::fs::File::from(control_master);
        let err = master_probe
            .read(&mut [0; 1])
            .expect_err("master read after final slave close reports EIO");
        assert_eq!(err.raw_os_error(), Some(libc::EIO));
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
            pane_id: test_pane_id(),
            master_fd: owned,
            on_read: Box::new(move |bytes| {
                read_tx
                    .send(Bytes::copy_from_slice(bytes))
                    .expect("read callback receiver alive");
                PtyReadResult::empty()
            }),
            on_reader_exit: Box::new(|_| {}),
            core_broken: Box::new(|| false),
        })
        .expect("actor spawn");

        let marker = Bytes::from_static(b"queued-input\r");
        handle
            .try_write_user_input(marker.clone())
            .expect("input accepted");

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

        let mut received_input = vec![0; prefilled + marker.len()];
        peer.read_exact(&mut received_input)
            .expect("peer receives prefill and queued input");
        assert!(received_input[..prefilled].iter().all(|byte| *byte == 0xAA));
        assert_eq!(&received_input[prefilled..], marker.as_ref());
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
    fn resize_reply_entries_have_distinct_ids_at_the_same_sequence_order() {
        let mut inbox = PtyIoInbox::default();
        assert_eq!(
            inbox.replace_resize(
                shepr_core::geometry::PaneGeometry::with_cell(
                    80,
                    24,
                    shepr_core::geometry::CellPx::new(8, 16)
                ),
                vec![Bytes::from_static(b"one"), Bytes::from_static(b"two")],
            ),
            None
        );
        let resize = inbox.latest_resize.take().expect("resize was queued");
        inbox.insert_resize_replies(resize.order, resize.terminal_responses);

        let first = inbox.entries.front().expect("first reply was inserted");
        let second = inbox.entries.get(1).expect("second reply was inserted");
        assert_eq!(first.order, second.order);
        assert_ne!(first.id, second.id);
        assert_eq!(inbox.writable_index(Some(second.id)), Some(1));
    }

    #[test]
    fn terminal_reply_overflow_is_counted_and_reported_once() {
        let mut inbox = PtyIoInbox::default();
        assert!(inbox.reserve(ACTOR_INBOX_MAX_BYTES, 0));

        assert_eq!(
            inbox.push_terminal_response(Bytes::from_static(b"first")),
            ResponsePush::Dropped { first: true }
        );
        assert_eq!(
            inbox.push_terminal_response(Bytes::from_static(b"second")),
            ResponsePush::Dropped { first: false }
        );
        assert_eq!(inbox.terminal_response_drops, 2);
        assert_eq!(inbox.mark_terminal_response_drops_reported(), Some(2));
        assert_eq!(inbox.mark_terminal_response_drops_reported(), None);
        assert_eq!(
            inbox.push_terminal_response(Bytes::from_static(b"third")),
            ResponsePush::Dropped { first: false }
        );
        assert_eq!(inbox.mark_terminal_response_drops_reported(), Some(3));
    }

    #[test]
    fn resize_keeps_latest_request_without_reordering_queued_replies() {
        let (_runner, handle, _peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        handle.write_terminal_response(|| Some(Bytes::from_static(b"before")));
        handle.resize(
            shepr_core::geometry::PaneGeometry::with_cell(
                80,
                20,
                shepr_core::geometry::CellPx::new(8, 16),
            ),
            || vec![Bytes::from_static(b"old")],
        );
        handle.write_terminal_response(|| Some(Bytes::from_static(b"middle")));
        handle.resize(
            shepr_core::geometry::PaneGeometry::with_cell(
                120,
                40,
                shepr_core::geometry::CellPx::new(9, 18),
            ),
            || vec![Bytes::from_static(b"new")],
        );

        let inbox = crate::locks::lock_auxiliary(&handle.inbox);
        assert_eq!(
            inbox
                .latest_resize
                .as_ref()
                .map(|resize| resize.resize.geometry),
            Some(shepr_core::geometry::PaneGeometry::with_cell(
                120,
                40,
                shepr_core::geometry::CellPx::new(9, 18)
            ))
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
            .filter_map(|entry| match &entry.write {
                PendingWrite::TerminalResponse(bytes) => Some(bytes.as_ref()),
                PendingWrite::User(_) => None,
            })
            .collect();
        assert_eq!(responses, [b"before".as_slice(), b"middle".as_slice()]);
    }

    #[test]
    fn appearance_transition_report_precedes_query_of_new_scheme() {
        let light = Arc::new(AtomicBool::new(false));
        let query_light = Arc::clone(&light);
        let (runner, handle, mut peer) = actor_test_parts(Box::new(move |_| {
            PtyReadResult::Effects(PtyReadEffects {
                terminal_responses: vec![if query_light.load(Ordering::Acquire) {
                    Bytes::from_static(b"query-light")
                } else {
                    Bytes::from_static(b"query-dark")
                }],
                after_response_order: None,
            })
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
            assert_eq!(runner.read_chunk(), ReadOutcome::Data);
            runner
        });
        continue_tx.send(()).expect("release appearance report");
        appearance.join().expect("appearance thread joins");
        let runner = reader.join().expect("reader thread joins");

        let inbox = crate::locks::lock_auxiliary(&runner.inbox);
        let responses: Vec<_> = inbox
            .entries
            .iter()
            .filter_map(|entry| match &entry.write {
                PendingWrite::TerminalResponse(bytes) => Some(bytes.as_ref()),
                PendingWrite::User(_) => None,
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
        runner.io.resize_pty = Box::new(|_, _| Ok(()));
        handle.write_terminal_response(|| Some(Bytes::from_static(b"earlier")));
        handle.resize(
            shepr_core::geometry::PaneGeometry::with_cell(
                100,
                40,
                shepr_core::geometry::CellPx::new(9, 18),
            ),
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
    fn failed_resize_is_not_retried_and_its_replies_keep_their_place() {
        let (mut runner, handle, mut peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        runner.io.resize_pty = Box::new({
            let calls = Arc::clone(&calls);
            move |_, _| {
                calls.fetch_add(1, Ordering::AcqRel);
                Err(std::io::Error::other("ioctl failure"))
            }
        });
        handle.write_terminal_response(|| Some(Bytes::from_static(b"earlier")));
        handle.resize(
            shepr_core::geometry::PaneGeometry::with_cell(
                100,
                40,
                shepr_core::geometry::CellPx::new(9, 18),
            ),
            || vec![Bytes::from_static(b"reply")],
        );
        handle
            .try_write_user_input(Bytes::from_static(b"typed"))
            .expect("input queues");

        runner.pump().expect("pump with a failing resize");
        let mut received = [0; 17];
        peer.read_exact(&mut received)
            .expect("replies and input go out despite the failed ioctl");
        assert_eq!(&received, b"earlierreplytyped");
        assert!(
            crate::locks::lock_auxiliary(&runner.inbox)
                .latest_resize
                .is_none(),
            "a failed resize is dropped, not kept for a retry"
        );
        assert!(runner.resize_failure_logged);

        runner.pump().expect("idle pump");
        assert_eq!(calls.load(Ordering::Acquire), 1, "the ioctl is not retried");
    }

    #[test]
    fn resize_is_applied_while_earlier_input_waits_on_an_unread_pty() {
        let (mut runner, handle, _peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        runner.io.resize_pty = Box::new({
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
            shepr_core::geometry::PaneGeometry::with_cell(
                100,
                40,
                shepr_core::geometry::CellPx::new(9, 18),
            ),
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
            pane_id: test_pane_id(),
            wake,
            inbox: Arc::new(Mutex::new(PtyIoInbox::default())),
            response_order: Arc::new(Mutex::new(())),
        };
        let paste = Bytes::from(vec![b'p'; ACTOR_INBOX_MAX_BYTES * 2]);
        handle
            .try_write_user_input(paste.clone())
            .expect("a large paste reaches an idle pane");
        match handle.try_write_user_input(Bytes::from_static(b"k")) {
            Err(ChildIoSendError::Full(bytes)) => assert_eq!(bytes, "k"),
            other => panic!("expected a full queue, got {other:?}"),
        }
        match handle.try_write_user_input(paste) {
            Err(ChildIoSendError::Full(bytes)) => {
                assert_eq!(bytes.len(), ACTOR_INBOX_MAX_BYTES * 2);
            }
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
            pane_id: test_pane_id(),
            master_fd: owned,
            // Every read is a query that earns a reply, as for a child that
            // prints DA1 or DSR in a loop.
            on_read: Box::new(move |bytes| {
                // The actor thread keeps reading after the test has counted
                // enough and dropped the receiver; later reads need no count.
                read_tx.send(bytes.len()).ok();
                PtyReadResult::Effects(PtyReadEffects {
                    terminal_responses: vec![Bytes::from(vec![b'r'; REPLY_LEN])],
                    after_response_order: None,
                })
            }),
            on_reader_exit: Box::new(|_| {}),
            core_broken: Box::new(|| false),
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
            .map(|entry| match &entry.write {
                PendingWrite::TerminalResponse(bytes) => bytes.len(),
                PendingWrite::User(_) => 0,
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
    fn finished_writes_release_their_reservation() {
        let (mut runner, handle, mut peer) = actor_test_parts(Box::new(|_| PtyReadResult::empty()));
        handle
            .try_write_user_input(Bytes::from_static(b"again\r"))
            .expect("input queues");
        runner.pump().expect("input writes");
        let mut written = [0; 6];
        peer.read_exact(&mut written).expect("peer receives input");
        assert_eq!(&written, b"again\r");
        let inbox = crate::locks::lock_auxiliary(&runner.inbox);
        assert_eq!((inbox.pending_bytes, inbox.pending_items), (0, 0));
        assert!(inbox.entries.is_empty());
    }
}
