//! Supervised child processes: one bounded run of a host utility, with its
//! input written, its outputs drained and its lifetime ended by a deadline.
//!
//! Every run is admitted against a [`ChildBudget`] the calling subsystem owns,
//! and the reservation it takes is held until the run has met every ownership
//! obligation: spawn failed without leaving a child, or the child is reaped
//! and every stream thread has finished. A child that outlives its SIGKILL (a
//! process in an uninterruptible kernel wait) goes to one shared background
//! reaper together with its reservation, so a stuck child keeps its slot and
//! the subsystem cannot pile up processes. Budgets are partitions: one
//! subsystem's stuck children never take another's capacity.
//!
//! A run is owned by its own supervisor thread. The caller waits for the
//! report only until the deadline plus [`SUPERVISED_RESULT_GRACE`]; past that
//! it gets [`ChildEnd::TimedOut`] with no stream reports, and the supervisor
//! keeps the reservation while it finishes cleanup. So the caller stops
//! waiting by the deadline even when `Command::spawn` or the kernel is stuck;
//! the process itself is gone only once the kernel lets it go.
//!
//! The child is placed in its own process group before exec. The group is
//! signalled only while its leader is unreaped (observed with `WNOWAIT`), so
//! the group id cannot have been reused. Descendants that leave the group are
//! not contained: this supervises cooperative host utilities.
//!
//! Extra output pipes keep the descriptor number they have in the parent; the
//! child only clears close-on-exec on them, so nothing the spawn machinery
//! itself holds (its exec-error pipe included) can be overwritten. The caller
//! learns the numbers before it builds the command.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::limits::{
    SUPERVISED_CANCEL_SLICE, SUPERVISED_KILL_REAP_GRACE, SUPERVISED_POLL_INTERVAL,
    SUPERVISED_READ_CHUNK_BYTES, SUPERVISED_REAPER_POLL_INTERVAL, SUPERVISED_RESULT_GRACE,
};

/// A subsystem's share of supervised children: at most `cap` runs, counting
/// those starting, running and waiting to be reaped. Declared as a `static` by
/// the subsystem that owns it.
pub struct ChildBudget {
    name: &'static str,
    cap: usize,
    held: Mutex<usize>,
}

impl ChildBudget {
    pub const fn new(name: &'static str, cap: usize) -> Self {
        Self {
            name,
            cap,
            held: Mutex::new(0),
        }
    }

    /// Runs currently holding a reservation.
    pub fn outstanding(&'static self) -> usize {
        *self.lock()
    }

    fn lock(&self) -> MutexGuard<'_, usize> {
        // The guarded value is a counter that is only incremented or
        // decremented whole, so a panic elsewhere leaves it consistent.
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn try_reserve(&'static self) -> Option<Reservation> {
        let mut held = self.lock();
        if *held >= self.cap {
            return None;
        }
        *held += 1;
        Some(Reservation { budget: self })
    }
}

/// One admitted run's slot, released on drop.
struct Reservation {
    budget: &'static ChildBudget,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut held = self.budget.lock();
        *held = held.saturating_sub(1);
    }
}

/// Which child descriptor a captured stream reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStream {
    Stdout,
    Stderr,
    /// The extra pipe with this index; its descriptor number in the child is
    /// handed to the run's `prepare` at the same index.
    Extra(usize),
}

/// What a stream does once it has captured its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    /// Stop the run: its output is unusable past the cap.
    Terminate,
    /// Keep the captured prefix and discard the rest, draining to the end so
    /// the child never blocks on a full pipe.
    Discard,
}

#[derive(Debug, Clone, Copy)]
pub struct StreamSpec {
    pub stream: ChildStream,
    pub cap: usize,
    pub overflow: Overflow,
}

/// One supervised run.
pub struct SupervisedRun {
    pub budget: &'static ChildBudget,
    /// How many extra output pipes the child gets beside stdout and stderr.
    pub extra_pipes: usize,
    /// The streams captured, each reported in this order. Stdout and stderr
    /// not listed go to `/dev/null`; every extra pipe must be listed once.
    pub streams: Vec<StreamSpec>,
    /// Absolute deadline, sampled by the caller before this run starts.
    pub deadline: Instant,
}

/// The command and its input, built once the extra pipes' child descriptor
/// numbers are known.
pub struct PreparedChild {
    pub command: Command,
    /// Bytes written to the child's stdin, which is then closed; the buffer
    /// is wiped however the run ends. `None` gives the child `/dev/null`.
    pub stdin: Option<Vec<u8>>,
}

/// What one captured stream delivered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamReport {
    /// The captured prefix, at most the stream's cap.
    pub bytes: Vec<u8>,
    /// More bytes arrived than the cap allowed.
    pub overflowed: bool,
    /// The stream reached its end.
    pub eof: bool,
    /// A read failed (other than the deadline).
    pub error: Option<io::ErrorKind>,
}

/// How a run ended.
#[derive(Debug)]
pub enum ChildEnd {
    /// The child exited, stdin was delivered and every stream reached its end.
    Exited(ExitStatus),
    /// The deadline passed first; the process group was killed. Also what
    /// the caller gets when the supervisor did not report within the grace.
    TimedOut,
    /// A [`Overflow::Terminate`] stream passed its cap; the group was killed.
    Overflowed,
    /// The budget had no free slot; nothing was started.
    Exhausted,
    /// The child could not be started.
    Spawn(io::Error),
    /// Supervising the child failed: a pipe, a stream or stdin delivery
    /// failed, a supervision thread could not start or panicked, or the run
    /// was invalid. The group was killed.
    Supervision(io::Error),
}

#[derive(Debug)]
pub struct ChildReport {
    pub end: ChildEnd,
    /// One report per requested stream, in request order, also when the run
    /// failed: a complete header block survives a body that timed out. Empty
    /// reports when the supervisor did not answer in time.
    pub streams: Vec<StreamReport>,
    /// Writing stdin failed before all of it was delivered.
    pub stdin_error: Option<io::ErrorKind>,
}

impl ChildReport {
    fn empty(end: ChildEnd, streams: usize) -> Self {
        Self {
            end,
            streams: vec![StreamReport::default(); streams],
            stdin_error: None,
        }
    }
}

/// Runs a child under supervision. `prepare` receives the extra pipes' child
/// descriptor numbers, in index order, and builds the command (program,
/// arguments, environment, working directory) and its stdin; the command's
/// stdio and process group are set here.
pub fn run_supervised(
    run: SupervisedRun,
    prepare: impl FnOnce(&[RawFd]) -> PreparedChild + Send + 'static,
) -> ChildReport {
    let stream_count = run.streams.len();
    if let Err(error) = validate_streams(&run) {
        return ChildReport::empty(ChildEnd::Supervision(error), stream_count);
    }
    let Some(reservation) = run.budget.try_reserve() else {
        // Queued children may be waiting on a reaper that failed to start;
        // try again here, since no new hand-over will come while full.
        restart_reaper_if_needed();
        crate::structured_log!(
            WARN,
            event = child.supervise,
            outcome = Exhausted,
            budget = run.budget.name,
            "every supervised child slot of this subsystem is taken; not starting another"
        );
        return ChildReport::empty(ChildEnd::Exhausted, stream_count);
    };

    let mut extra = Vec::with_capacity(run.extra_pipes);
    for _ in 0..run.extra_pipes {
        match cloexec_pipe() {
            Ok(pipe) => extra.push(pipe),
            Err(error) => return ChildReport::empty(ChildEnd::Supervision(error), stream_count),
        }
    }
    let child_fds: Vec<RawFd> = extra.iter().map(|(_, write)| write.as_raw_fd()).collect();

    let give_up = run.deadline + SUPERVISED_RESULT_GRACE;
    let (sender, receiver) = mpsc::sync_channel(1);
    let started = std::thread::Builder::new()
        .name("child-supervise".into())
        .spawn(move || {
            // Preparation can block too, so it runs inside the attempt.
            let prepared = prepare(&child_fds);
            let report = supervise_run(&run, prepared, extra, &child_fds, reservation);
            // The caller may have stopped waiting; then the report is dropped.
            sender.send(report).ok();
        });
    if let Err(error) = started {
        return ChildReport::empty(ChildEnd::Supervision(error), stream_count);
    }
    // clock-io-ok: the caller's wait is bounded by real time.
    let wait = give_up.saturating_duration_since(Instant::now());
    receiver
        .recv_timeout(wait)
        .unwrap_or_else(|_| ChildReport::empty(ChildEnd::TimedOut, stream_count))
}

fn validate_streams(run: &SupervisedRun) -> io::Result<()> {
    let invalid = |message| Err(io::Error::new(io::ErrorKind::InvalidInput, message));
    for (index, spec) in run.streams.iter().enumerate() {
        if run.streams[..index]
            .iter()
            .any(|earlier| earlier.stream == spec.stream)
        {
            return invalid("a child stream is captured twice");
        }
        if let ChildStream::Extra(extra) = spec.stream
            && extra >= run.extra_pipes
        {
            return invalid("a captured extra pipe does not exist");
        }
    }
    let listed = run
        .streams
        .iter()
        .filter(|spec| matches!(spec.stream, ChildStream::Extra(_)))
        .count();
    if listed != run.extra_pipes {
        return invalid("every extra pipe must be captured");
    }
    Ok(())
}

/// The whole run on the supervisor thread: spawn, stream threads, the wait,
/// and cleanup. Holds the reservation until its obligations are met.
fn supervise_run(
    run: &SupervisedRun,
    prepared: PreparedChild,
    extra: Vec<(OwnedFd, OwnedFd)>,
    child_fds: &[RawFd],
    reservation: Reservation,
) -> ChildReport {
    let stream_count = run.streams.len();
    let PreparedChild { mut command, stdin } = prepared;
    // Wiped on every path from here: spawn failure, a passed deadline, a
    // writer that cannot start, or the writer finishing.
    let stdin = stdin.map(WipedBytes);
    let wants = |stream: ChildStream| run.streams.iter().any(|spec| spec.stream == stream);
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if wants(ChildStream::Stdout) {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(if wants(ChildStream::Stderr) {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    install_child_setup(&mut command, child_fds.to_vec());

    // clock-io-ok: an attempt already past its deadline starts nothing.
    if Instant::now() >= run.deadline {
        return ChildReport::empty(ChildEnd::TimedOut, stream_count);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return ChildReport::empty(ChildEnd::Spawn(error), stream_count),
    };
    // The parent's write ends must close, or the readers never see the end.
    let mut extra_reads: Vec<Option<OwnedFd>> = extra
        .into_iter()
        .map(|(read, write)| {
            drop(write);
            Some(read)
        })
        .collect();

    let shared = Arc::new(RunFlags::default());
    let mut drains: Vec<Option<JoinHandle<StreamReport>>> = Vec::with_capacity(stream_count);
    let mut setup_error = None;
    for spec in &run.streams {
        let source: Option<OwnedFd> = match spec.stream {
            ChildStream::Stdout => child.stdout.take().map(OwnedFd::from),
            ChildStream::Stderr => child.stderr.take().map(OwnedFd::from),
            ChildStream::Extra(index) => extra_reads.get_mut(index).and_then(Option::take),
        };
        let Some(source) = source else {
            drains.push(None);
            continue;
        };
        match spawn_drain(source, *spec, run.deadline, &shared) {
            Ok(handle) => drains.push(Some(handle)),
            Err(error) => {
                drains.push(None);
                setup_error.get_or_insert(error);
            }
        }
    }
    let writer = match (stdin, child.stdin.take()) {
        (Some(bytes), Some(pipe)) => match spawn_writer(pipe, bytes, run.deadline, &shared) {
            Ok(handle) => Some(handle),
            Err(error) => {
                setup_error.get_or_insert(error);
                None
            }
        },
        _ => None,
    };

    let end = match setup_error {
        Some(error) => {
            kill_group(&child);
            ChildEnd::Supervision(error)
        }
        None => supervise(&mut child, &drains, writer.as_ref(), run.deadline, &shared),
    };

    // Stop the stream threads that are still waiting; each ends within one
    // cancel slice, or at once once the killed group's pipe ends close.
    let finished = matches!(end, ChildEnd::Exited(_));
    if !finished {
        shared.cancel.store(true, Ordering::SeqCst);
    }
    let streams = drains
        .into_iter()
        .map(|drain| match drain {
            Some(handle) => handle.join().unwrap_or_else(|_| StreamReport {
                error: Some(io::ErrorKind::Other),
                ..StreamReport::default()
            }),
            None => StreamReport::default(),
        })
        .collect();
    let stdin_error = writer.and_then(|handle| handle.join().unwrap_or(Some(io::ErrorKind::Other)));

    if finished {
        drop(reservation);
    } else {
        reap_or_hand_off(child, reservation);
    }
    ChildReport {
        end,
        streams,
        stdin_error,
    }
}

/// What the supervisor and its stream threads share.
#[derive(Default)]
struct RunFlags {
    /// Set by the supervisor once the run is over: threads stop waiting.
    cancel: AtomicBool,
    /// Set by a drain whose terminating cap was passed.
    overflowed: AtomicBool,
    /// Set by a stream thread that failed or panicked.
    failed: AtomicBool,
}

/// Marks the run failed if its thread unwinds or returns early with a
/// failure; disarmed only on a clean finish.
struct FailOnDrop<'a> {
    flags: &'a RunFlags,
    armed: bool,
}

impl Drop for FailOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.flags.failed.store(true, Ordering::SeqCst);
        }
    }
}

/// Waits for the child and its streams, killing the group on the deadline, a
/// terminating overflow or a failed stream thread. Reaps the child only on a
/// clean finish.
fn supervise(
    child: &mut Child,
    drains: &[Option<JoinHandle<StreamReport>>],
    writer: Option<&JoinHandle<Option<io::ErrorKind>>>,
    deadline: Instant,
    flags: &RunFlags,
) -> ChildEnd {
    loop {
        if flags.overflowed.load(Ordering::SeqCst) {
            kill_group(child);
            return ChildEnd::Overflowed;
        }
        if flags.failed.load(Ordering::SeqCst) {
            kill_group(child);
            return ChildEnd::Supervision(io::Error::other(
                "a supervised child's stream or input failed",
            ));
        }
        // clock-io-ok: the supervisor bounds real process and pipe IO.
        if Instant::now() >= deadline {
            kill_group(child);
            return ChildEnd::TimedOut;
        }
        match leader_exited(child) {
            Ok(true) => {
                let streams_done = drains
                    .iter()
                    .all(|drain| drain.as_ref().is_none_or(JoinHandle::is_finished));
                let writer_done = writer.is_none_or(JoinHandle::is_finished);
                // A thread that finished may have failed as it did; the flags
                // are checked once more before the leader is reaped.
                if streams_done
                    && writer_done
                    && !flags.failed.load(Ordering::SeqCst)
                    && !flags.overflowed.load(Ordering::SeqCst)
                {
                    return match child.wait() {
                        Ok(status) => ChildEnd::Exited(status),
                        Err(error) => ChildEnd::Supervision(error),
                    };
                }
                // A descendant still holds a pipe end: wait for it until the
                // deadline, while the zombie leader keeps the group id ours.
            }
            Ok(false) => {}
            Err(error) => {
                kill_group(child);
                return ChildEnd::Supervision(error);
            }
        }
        std::thread::sleep(SUPERVISED_POLL_INTERVAL);
    }
}

/// Whether the child has exited, without reaping it.
fn leader_exited(child: &Child) -> io::Result<bool> {
    // SAFETY: zero is a valid bit pattern for siginfo_t, which waitid fills.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waitid writes only into `info`, a live exclusive local.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: waitid succeeded, so si_pid is initialized (zero when no child
    // has changed state).
    Ok(unsafe { info.si_pid() } != 0)
}

/// SIGKILLs the child's process group. Called only while the leader is
/// unreaped, so the group id still names this child's group.
fn kill_group(child: &Child) {
    let Ok(pgid) = libc::pid_t::try_from(child.id()) else {
        return;
    };
    // SAFETY: kill(2) takes plain integers and touches no memory.
    if unsafe { libc::kill(-pgid, libc::SIGKILL) } < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            tracing::debug!(%error, "failed to signal a supervised child's process group");
        }
    }
}

/// Process group, and close-on-exec cleared on the extra pipes' write ends
/// so they survive exec at the numbers the caller was told.
fn install_child_setup(command: &mut Command, inherited: Vec<RawFd>) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs in the forked child and only calls setpgid(2)
    // and fcntl(2), which are async-signal-safe; it reads the `inherited`
    // vector allocated before the fork and allocates nothing.
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            for &fd in &inherited {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// A close-on-exec pipe whose both ends sit above the standard descriptors:
/// with a standard descriptor closed, `pipe2` could hand out 0, 1 or 2, which
/// the child's stdio setup would then overwrite.
fn cloexec_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: pipe2 writes two descriptors into `fds`, a live local array.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 succeeded, so both descriptors are open and owned by us.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    Ok((above_standard(read)?, above_standard(write)?))
}

fn above_standard(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(fd);
    }
    // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor at or above the
    // minimum, or fails; it touches no memory.
    let moved = unsafe {
        libc::fcntl(
            fd.as_raw_fd(),
            libc::F_DUPFD_CLOEXEC,
            libc::STDERR_FILENO + 1,
        )
    };
    if moved < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl succeeded, so `moved` is a new descriptor we own; the low
    // one closes when `fd` drops.
    Ok(unsafe { OwnedFd::from_raw_fd(moved) })
}

fn spawn_drain(
    source: OwnedFd,
    spec: StreamSpec,
    deadline: Instant,
    flags: &Arc<RunFlags>,
) -> io::Result<JoinHandle<StreamReport>> {
    let flags = Arc::clone(flags);
    std::thread::Builder::new()
        .name("child-drain".into())
        .spawn(move || {
            let mut guard = FailOnDrop {
                flags: &flags,
                armed: true,
            };
            let report = drain(source, spec, deadline, &flags);
            guard.armed = report.error.is_some();
            report
        })
}

/// Reads one stream to its end, its deadline or cancellation, keeping at most
/// `spec.cap` bytes. Each wait is one cancel slice at most, so cancellation and
/// the deadline are checked even while data keeps arriving.
fn drain(source: OwnedFd, spec: StreamSpec, deadline: Instant, flags: &RunFlags) -> StreamReport {
    let mut report = StreamReport::default();
    let mut file = std::fs::File::from(source);
    let mut chunk = [0_u8; SUPERVISED_READ_CHUNK_BYTES];
    loop {
        if flags.cancel.load(Ordering::SeqCst) {
            return report;
        }
        // clock-io-ok: the reader runs against the real clock it bounds.
        let Some(remaining) = crate::remaining_until(deadline, Instant::now()) else {
            return report;
        };
        match crate::poll_fd_readable(file.as_raw_fd(), remaining.min(SUPERVISED_CANCEL_SLICE)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                report.error = Some(error.kind());
                return report;
            }
        }
        match file.read(&mut chunk) {
            Ok(0) => {
                report.eof = true;
                return report;
            }
            Ok(count) => {
                let room = spec.cap.saturating_sub(report.bytes.len());
                report.bytes.extend_from_slice(&chunk[..count.min(room)]);
                if count > room {
                    report.overflowed = true;
                    if spec.overflow == Overflow::Terminate {
                        flags.overflowed.store(true, Ordering::SeqCst);
                        return report;
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                report.error = Some(error.kind());
                return report;
            }
        }
    }
}

/// Stdin bytes, wiped when dropped on whatever path drops them: callers pass
/// secrets through stdin (a request config with a bearer token).
struct WipedBytes(Vec<u8>);

impl Drop for WipedBytes {
    fn drop(&mut self) {
        for byte in &mut self.0 {
            // SAFETY: `byte` is a valid, exclusive reference into the buffer.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

fn spawn_writer(
    pipe: ChildStdin,
    bytes: WipedBytes,
    deadline: Instant,
    flags: &Arc<RunFlags>,
) -> io::Result<JoinHandle<Option<io::ErrorKind>>> {
    let flags = Arc::clone(flags);
    std::thread::Builder::new()
        .name("child-stdin".into())
        .spawn(move || {
            let mut guard = FailOnDrop {
                flags: &flags,
                armed: true,
            };
            let error = write_input(pipe, &bytes.0, deadline, &flags);
            drop(bytes);
            // Input cut short by the run ending is not a failure of its own.
            guard.armed = error.is_some() && !flags.cancel.load(Ordering::SeqCst);
            error
        })
}

/// Writes `bytes` with nonblocking writes, waiting for room only until the
/// deadline or cancellation, then closes the pipe. A child that stops reading
/// cannot hold this thread past the deadline.
fn write_input(
    mut pipe: ChildStdin,
    bytes: &[u8],
    deadline: Instant,
    flags: &RunFlags,
) -> Option<io::ErrorKind> {
    if let Err(error) = crate::set_nonblocking(pipe.as_raw_fd()) {
        return Some(error.kind());
    }
    let mut written = 0;
    while written < bytes.len() {
        if flags.cancel.load(Ordering::SeqCst) {
            return Some(io::ErrorKind::Interrupted);
        }
        // clock-io-ok: the writer runs against the real clock it bounds.
        let Some(remaining) = crate::remaining_until(deadline, Instant::now()) else {
            return Some(io::ErrorKind::TimedOut);
        };
        match crate::poll_fd(
            pipe.as_raw_fd(),
            libc::POLLOUT,
            remaining.min(SUPERVISED_CANCEL_SLICE),
        ) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Some(error.kind()),
        }
        match pipe.write(&bytes[written..]) {
            Ok(count) => written += count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => return Some(error.kind()),
        }
    }
    None
}

/// Waits up to the kill grace for a killed child to be reaped, then hands it
/// and its reservation to the shared reaper.
fn reap_or_hand_off(mut child: Child, reservation: Reservation) {
    // clock-io-ok: the grace is a real-time bound on the supervisor.
    let give_up = Instant::now() + SUPERVISED_KILL_REAP_GRACE;
    loop {
        match reap_state(&mut child) {
            ReapState::Reaped => return,
            ReapState::Running => {}
            ReapState::Uncertain => break,
        }
        // clock-io-ok: as above.
        if Instant::now() >= give_up {
            break;
        }
        std::thread::sleep(SUPERVISED_POLL_INTERVAL);
    }
    hand_to_reaper(child, reservation);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReapState {
    /// Reaped now, or proven to have no child left to reap (ECHILD).
    Reaped,
    Running,
    /// The wait failed in a way that proves nothing; ownership is kept.
    Uncertain,
}

fn reap_state(child: &mut Child) -> ReapState {
    loop {
        return match child.try_wait() {
            Ok(Some(_)) => ReapState::Reaped,
            Ok(None) => ReapState::Running,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => ReapState::Reaped,
            Err(error) => {
                tracing::debug!(%error, pid = child.id(), "could not reap a supervised child; keeping it");
                ReapState::Uncertain
            }
        };
    }
}

/// Killed children that outlived their grace, each with the reservation it
/// keeps until reaped, and whether the reaper thread is running.
struct Reaper {
    children: Vec<(Child, Reservation)>,
    running: bool,
}

static REAPER: Mutex<Reaper> = Mutex::new(Reaper {
    children: Vec::new(),
    running: false,
});

fn lock_reaper() -> MutexGuard<'static, Reaper> {
    // Every critical section is a push, a retain or a flag, so a panic on the
    // holder leaves the value whole.
    REAPER.lock().unwrap_or_else(PoisonError::into_inner)
}

fn hand_to_reaper(child: Child, reservation: Reservation) {
    let mut reaper = lock_reaper();
    tracing::debug!(
        pid = child.id(),
        budget = reservation.budget.name,
        "handed a killed supervised child to the reaper"
    );
    reaper.children.push((child, reservation));
    start_reaper(&mut reaper);
}

/// Starts the reaper thread for queued children if it is not running, as
/// after an earlier start failed.
fn restart_reaper_if_needed() {
    let mut reaper = lock_reaper();
    if !reaper.children.is_empty() {
        start_reaper(&mut reaper);
    }
}

fn start_reaper(reaper: &mut Reaper) {
    if reaper.running {
        return;
    }
    reaper.running = true;
    if let Err(error) = std::thread::Builder::new()
        // Linux exposes at most 15 bytes through `pthread_setname_np`.
        .name("child-reaper".into())
        .spawn(reap_until_empty)
    {
        // The children stay queued with their reservations; the next
        // hand-over or exhausted admission starts the thread again.
        reaper.running = false;
        tracing::debug!(%error, "could not start the supervised child reaper");
    }
}

fn reap_until_empty() {
    loop {
        std::thread::sleep(SUPERVISED_REAPER_POLL_INTERVAL);
        let mut reaper = lock_reaper();
        // Only a child proven reaped, or proven absent, is dropped, which
        // releases its reservation; anything uncertain is kept.
        reaper
            .children
            .retain_mut(|(child, _)| reap_state(child) != ReapState::Reaped);
        if reaper.children.is_empty() {
            reaper.running = false;
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::fixture::{Held, Step, stand_in};
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(30);

    fn stdout_spec(cap: usize, overflow: Overflow) -> StreamSpec {
        StreamSpec {
            stream: ChildStream::Stdout,
            cap,
            overflow,
        }
    }

    fn run(
        budget: &'static ChildBudget,
        program: &std::path::Path,
        stdin: Option<Vec<u8>>,
        streams: Vec<StreamSpec>,
        timeout: Duration,
    ) -> ChildReport {
        let extra_pipes = streams
            .iter()
            .filter(|spec| matches!(spec.stream, ChildStream::Extra(_)))
            .count();
        let program = program.to_path_buf();
        run_supervised(
            SupervisedRun {
                budget,
                extra_pipes,
                streams,
                deadline: Instant::now() + timeout,
            },
            move |_| PreparedChild {
                command: crate::child_command(&program, std::path::Path::new("/")),
                stdin,
            },
        )
    }

    fn wait_until_released(budget: &'static ChildBudget) {
        let deadline = Instant::now() + WAIT;
        while budget.outstanding() > 0 {
            assert!(Instant::now() < deadline, "reservation was never released");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn stdin_is_delivered_and_stdout_captured_to_its_end() {
        static BUDGET: ChildBudget = ChildBudget::new("test", 1);
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("supervised-cat");
        let cat = stand_in(root.path(), "cat", &[Step::Cat]);
        let report = run(
            &BUDGET,
            &cat,
            Some(b"config line\n".to_vec()),
            vec![stdout_spec(64, Overflow::Terminate)],
            WAIT,
        );
        assert!(matches!(report.end, ChildEnd::Exited(status) if status.success()));
        assert_eq!(report.streams[0].bytes, b"config line\n");
        assert!(report.streams[0].eof);
        assert!(!report.streams[0].overflowed);
        assert_eq!(report.stdin_error, None);
        wait_until_released(&BUDGET);
    }

    #[test]
    fn an_extra_pipe_reaches_the_child_at_the_number_it_was_told() {
        static BUDGET: ChildBudget = ChildBudget::new("test", 1);
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("supervised-fd");
        let root_path = root.path().to_path_buf();
        let report = run_supervised(
            SupervisedRun {
                budget: &BUDGET,
                extra_pipes: 1,
                streams: vec![
                    stdout_spec(64, Overflow::Terminate),
                    StreamSpec {
                        stream: ChildStream::Extra(0),
                        cap: 64,
                        overflow: Overflow::Terminate,
                    },
                ],
                deadline: Instant::now() + WAIT,
            },
            move |fds| {
                let program = stand_in(
                    &root_path,
                    "headers",
                    &[
                        Step::Print("body".into()),
                        Step::To(format!("/proc/self/fd/{}", fds[0]).into()),
                        Step::Print("HTTP/2 429\r\n".into()),
                    ],
                );
                PreparedChild {
                    command: crate::child_command(&program, std::path::Path::new("/")),
                    stdin: None,
                }
            },
        );
        assert!(
            matches!(report.end, ChildEnd::Exited(_)),
            "{:?}",
            report.end
        );
        assert_eq!(report.streams[0].bytes, b"body");
        assert_eq!(report.streams[1].bytes, b"HTTP/2 429\r\n");
        assert!(report.streams[1].eof);
    }

    #[test]
    fn invalid_stream_lists_are_refused_before_anything_starts() {
        static BUDGET: ChildBudget = ChildBudget::new("test", 1);
        let root = shepr_test_support::ScratchDir::new("supervised-invalid");
        for (extra_pipes, streams) in [
            (
                0,
                vec![
                    stdout_spec(1, Overflow::Discard),
                    stdout_spec(1, Overflow::Discard),
                ],
            ),
            (
                0,
                vec![StreamSpec {
                    stream: ChildStream::Extra(0),
                    cap: 1,
                    overflow: Overflow::Discard,
                }],
            ),
            (1, Vec::new()),
        ] {
            let missing = root.join("missing");
            let report = run_supervised(
                SupervisedRun {
                    budget: &BUDGET,
                    extra_pipes,
                    streams,
                    deadline: Instant::now() + WAIT,
                },
                move |_| PreparedChild {
                    command: crate::child_command(&missing, std::path::Path::new("/")),
                    stdin: None,
                },
            );
            assert!(
                matches!(report.end, ChildEnd::Supervision(_)),
                "{:?}",
                report.end
            );
            assert_eq!(BUDGET.outstanding(), 0);
        }
    }

    #[test]
    fn a_terminating_overflow_keeps_the_prefix_and_stops_the_run() {
        static BUDGET: ChildBudget = ChildBudget::new("test", 1);
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("supervised-overflow");
        let program = stand_in(
            root.path(),
            "flood",
            &[
                Step::Fill {
                    byte: b'x',
                    count: 1024 * 1024,
                },
                Step::Sleep(Duration::from_secs(30)),
            ],
        );
        let started = Instant::now();
        let report = run(
            &BUDGET,
            &program,
            None,
            vec![stdout_spec(16, Overflow::Terminate)],
            WAIT,
        );
        assert!(
            matches!(report.end, ChildEnd::Overflowed),
            "{:?}",
            report.end
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(report.streams[0].bytes, vec![b'x'; 16]);
        assert!(report.streams[0].overflowed);
        wait_until_released(&BUDGET);
    }

    #[test]
    fn a_discarding_overflow_drains_to_the_end() {
        static BUDGET: ChildBudget = ChildBudget::new("test", 1);
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("supervised-discard");
        let program = stand_in(
            root.path(),
            "flood",
            &[Step::Fill {
                byte: b'y',
                count: 1024 * 1024,
            }],
        );
        let report = run(
            &BUDGET,
            &program,
            None,
            vec![stdout_spec(8, Overflow::Discard)],
            WAIT,
        );
        assert!(matches!(report.end, ChildEnd::Exited(status) if status.success()));
        assert_eq!(report.streams[0].bytes, vec![b'y'; 8]);
        assert!(report.streams[0].overflowed);
        assert!(report.streams[0].eof);
    }

    #[test]
    fn the_deadline_kills_the_run_and_keeps_what_arrived_before_it() {
        static BUDGET: ChildBudget = ChildBudget::new("test", 1);
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("supervised-deadline");
        let program = stand_in(
            root.path(),
            "slow",
            &[
                Step::Print("partial".into()),
                Step::Sleep(Duration::from_secs(30)),
            ],
        );
        let started = Instant::now();
        let report = run(
            &BUDGET,
            &program,
            None,
            vec![stdout_spec(64, Overflow::Terminate)],
            Duration::from_millis(300),
        );
        assert!(matches!(report.end, ChildEnd::TimedOut), "{:?}", report.end);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(report.streams[0].bytes, b"partial");
        assert!(!report.streams[0].eof);
        wait_until_released(&BUDGET);
    }

    #[test]
    fn a_descendant_holding_a_pipe_is_killed_with_the_group_at_the_deadline() {
        static BUDGET: ChildBudget = ChildBudget::new("test", 1);
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("supervised-descendant");
        let program = stand_in(
            root.path(),
            "parent",
            &[Step::Spawn {
                argv0: "lingering".into(),
                sleep: Duration::from_secs(30),
                held: Held::Stdout,
            }],
        );
        let started = Instant::now();
        let report = run(
            &BUDGET,
            &program,
            None,
            vec![stdout_spec(64, Overflow::Terminate)],
            Duration::from_millis(300),
        );
        assert!(matches!(report.end, ChildEnd::TimedOut), "{:?}", report.end);
        assert!(started.elapsed() < Duration::from_secs(10));
        wait_until_released(&BUDGET);
    }

    #[test]
    fn a_full_budget_refuses_without_starting_anything() {
        static BUDGET: ChildBudget = ChildBudget::new("test", 0);
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("supervised-exhausted");
        let marker = root.join("started");
        let program = stand_in(
            root.path(),
            "marker",
            &[Step::To(marker.clone()), Step::Print("x".into())],
        );
        let report = run(
            &BUDGET,
            &program,
            None,
            vec![stdout_spec(8, Overflow::Terminate)],
            WAIT,
        );
        assert!(matches!(report.end, ChildEnd::Exhausted));
        assert_eq!(report.streams.len(), 1);
        assert!(!marker.try_exists().expect("stat marker"));
    }

    #[test]
    fn a_spawn_failure_releases_its_reservation() {
        static BUDGET: ChildBudget = ChildBudget::new("test", 1);
        let root = shepr_test_support::ScratchDir::new("supervised-spawn");
        let report = run(&BUDGET, &root.join("missing"), None, Vec::new(), WAIT);
        assert!(matches!(report.end, ChildEnd::Spawn(_)));
        wait_until_released(&BUDGET);
    }

    #[test]
    fn a_child_that_never_reads_stdin_cannot_hold_the_writer_past_the_deadline() {
        static BUDGET: ChildBudget = ChildBudget::new("test", 1);
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("supervised-stdin");
        let program = stand_in(root.path(), "deaf", &[Step::Sleep(Duration::from_secs(30))]);
        let started = Instant::now();
        let report = run(
            &BUDGET,
            &program,
            Some(vec![b'z'; 1024 * 1024]),
            Vec::new(),
            Duration::from_millis(300),
        );
        assert!(
            matches!(report.end, ChildEnd::TimedOut | ChildEnd::Supervision(_)),
            "{:?}",
            report.end
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        wait_until_released(&BUDGET);
    }

    #[test]
    fn a_child_that_exits_without_reading_its_input_is_not_a_clean_exit() {
        static BUDGET: ChildBudget = ChildBudget::new("test", 1);
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("supervised-stdin-exit");
        let program = stand_in(root.path(), "quick", &[Step::Exit(0)]);
        let report = run(
            &BUDGET,
            &program,
            Some(vec![b'z'; 1024 * 1024]),
            Vec::new(),
            WAIT,
        );
        assert!(
            matches!(report.end, ChildEnd::Supervision(_)),
            "{:?}",
            report.end
        );
        assert!(report.stdin_error.is_some());
        wait_until_released(&BUDGET);
    }
}
