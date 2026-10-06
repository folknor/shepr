use super::*;
use std::{
    os::fd::{AsRawFd, RawFd},
    path::Path,
    time::{Duration, Instant},
};

/// A positive Linux process id, representable by every pid-taking syscall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pid(std::num::NonZeroI32);

impl Pid {
    pub fn new(value: u32) -> Option<Self> {
        // A u32 that fits pid_t is never negative, so nonzero means positive.
        libc::pid_t::try_from(value)
            .ok()
            .and_then(std::num::NonZeroI32::new)
            .map(Self)
    }

    pub fn get(self) -> u32 {
        self.0.get().unsigned_abs()
    }

    pub fn as_pid_t(self) -> libc::pid_t {
        self.0.get()
    }
}

impl std::fmt::Display for Pid {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.get(), formatter)
    }
}

/// A process-group id. Its numeric equality to a pid does not make it a pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pgid(Pid);

impl Pgid {
    pub fn new(value: u32) -> Option<Self> {
        Pid::new(value).map(Self)
    }

    pub fn led_by(leader: Pid) -> Self {
        Self(leader)
    }

    pub fn leader_pid(self) -> Pid {
        self.0
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }

    pub fn as_pid_t(self) -> libc::pid_t {
        self.0.as_pid_t()
    }
}

/// A session id, allocated from its leader's process id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(Pid);

impl SessionId {
    pub fn new(value: u32) -> Option<Self> {
        Pid::new(value).map(Self)
    }

    pub fn of_leader(leader: Pid) -> Self {
        Self(leader)
    }

    pub fn leader_pid(self) -> Pid {
        self.0
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }

    pub fn as_pid_t(self) -> libc::pid_t {
        self.0.as_pid_t()
    }
}

/// Linux task states from proc_pid_stat(5), including historical kernel states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcState {
    Running,
    Sleeping,
    Uninterruptible,
    Zombie,
    Dead,
    Stopped,
    TracingStop,
    Idle,
    Paging,
    Wakekill,
    Waking,
    Parked,
}

impl ProcState {
    pub fn from_code(code: char) -> Option<Self> {
        Some(match code {
            'R' => Self::Running,
            'S' => Self::Sleeping,
            'D' => Self::Uninterruptible,
            'Z' => Self::Zombie,
            'X' | 'x' => Self::Dead,
            'T' => Self::Stopped,
            't' => Self::TracingStop,
            'I' => Self::Idle,
            'W' => Self::Paging,
            'K' => Self::Wakekill,
            'w' => Self::Waking,
            'P' => Self::Parked,
            _ => return None,
        })
    }

    pub fn is_finished(self) -> bool {
        matches!(self, Self::Zombie | Self::Dead)
    }

    pub fn is_stopped(self) -> bool {
        matches!(self, Self::Stopped | Self::TracingStop)
    }

    /// cmdline reads enter access_remote_vm and may block for an exiting
    /// process or one in uninterruptible sleep.
    pub fn allows_remote_memory_read(self) -> bool {
        !self.is_finished() && self != Self::Uninterruptible
    }
}

/// The stat fields used by process observation. Parent zero and foreground
/// group -1 denote absence; neither is a valid process or group id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: Pid,
    pub comm: String,
    pub state: ProcState,
    pub parent: Option<Pid>,
    pub process_group: Pgid,
    pub session: SessionId,
    pub tty_nr: i32,
    pub foreground_group: Option<Pgid>,
    pub start_ticks: u64,
}

impl ProcStat {
    /// Read a bounded stat record, validating its pid against the proc path.
    pub fn read(pid: Pid) -> std::io::Result<Self> {
        use std::io::Read;

        // limits-exempt: proc stat has a short comm and a fixed set of integer fields.
        const MAX_STAT_BYTES: u64 = 4096;
        let file = std::fs::File::open(format!("/proc/{pid}/stat"))?;
        let mut bytes = Vec::new();
        file.take(MAX_STAT_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_STAT_BYTES {
            return Err(invalid_stat());
        }
        // comm is a kernel byte string, not necessarily UTF-8. All fields we
        // interpret are ASCII; preserve best-effort display of the name.
        let stat = Self::parse(&String::from_utf8_lossy(&bytes)).ok_or_else(invalid_stat)?;
        if stat.pid != pid {
            return Err(invalid_stat());
        }
        Ok(stat)
    }

    pub fn parse(record: &str) -> Option<Self> {
        let open = record.find('(')?;
        let close = record.rfind(')')?;
        let pid = Pid::new(record.get(..open)?.trim().parse().ok()?)?;
        let comm = record.get(open + 1..close)?.to_owned();
        let mut fields = record.get(close + 1..)?.split_whitespace();
        let code = fields.next()?;
        let mut chars = code.chars();
        let state = ProcState::from_code(chars.next()?)?;
        if chars.next().is_some() {
            return None;
        }
        let parent_raw: u32 = fields.next()?.parse().ok()?;
        let parent = if parent_raw == 0 {
            None
        } else {
            Some(Pid::new(parent_raw)?)
        };
        let process_group = Pgid::new(fields.next()?.parse().ok()?)?;
        let session = SessionId::new(fields.next()?.parse().ok()?)?;
        let tty_nr = fields.next()?.parse().ok()?;
        let foreground_raw: i32 = fields.next()?.parse().ok()?;
        let foreground_group = match foreground_raw {
            -1 | 0 => None,
            value if value > 0 => Some(Pgid::new(u32::try_from(value).ok()?)?),
            _ => return None,
        };
        // tpgid is field 8; starttime is field 22.
        let start_ticks = fields.nth(13)?.parse().ok()?;
        Some(Self {
            pid,
            comm,
            state,
            parent,
            process_group,
            session,
            tty_nr,
            foreground_group,
            start_ticks,
        })
    }
}

fn invalid_stat() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "malformed process stat record",
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Hangup,
    Terminate,
    Kill,
}

fn signal_number(signal: Signal) -> libc::c_int {
    match signal {
        Signal::Hangup => libc::SIGHUP,
        Signal::Terminate => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    }
}

/// A handle on one specific process. A signal sent through it reaches that
/// process or nobody; a plain `kill(pid)` can land on an unrelated process
/// the kernel later gave the same pid.
#[derive(Debug)]
pub struct ProcessHandle {
    pid: Pid,
    pidfd: std::os::fd::OwnedFd,
}

impl ProcessHandle {
    /// Open a handle on the process that holds `pid` right now. Returns
    /// `None` if it is absent or a pidfd cannot be opened, without logging:
    /// a `/proc` scan can meet the same resource failure for every process.
    /// A caller that must report why uses [`Self::open_checked`].
    pub fn open(pid: Pid) -> Option<Self> {
        Self::open_checked(pid).ok().flatten()
    }

    /// [`Self::open`] with the failure kept: `Ok(None)` when no process holds
    /// `pid` (or it names a thread), `Err` when a pidfd could not be opened
    /// for a process that may exist. A pid alone is not a safe process
    /// identity, so either way there is no handle: fd exhaustion must refuse
    /// it rather than signal through a pid that may have been reused.
    pub fn open_checked(pid: Pid) -> std::io::Result<Option<Self>> {
        use std::os::fd::FromRawFd;

        let raw_pid = pid.as_pid_t();
        // SAFETY: pidfd_open(2) takes a pid and a flags word and returns a new
        // close-on-exec fd or -1; it reads and writes no memory of ours.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, raw_pid, 0_u32) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                // No such process, or `pid` names a thread, not a process.
                Some(libc::ESRCH | libc::EINVAL) => Ok(None),
                _ => Err(error),
            };
        }
        let fd = RawFd::try_from(fd).map_err(std::io::Error::other)?;
        // SAFETY: `fd` was just returned by pidfd_open and nothing else owns it.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        Ok(Some(Self { pid, pidfd: fd }))
    }

    pub fn process_id(&self) -> Pid {
        self.pid
    }

    /// Duplicate the pidfd for readiness polling.
    /// The returned descriptor has its own lifetime and can be registered
    /// with an async poller without exposing or transferring this handle's fd.
    pub fn try_clone_pidfd(&self) -> std::io::Result<std::os::fd::OwnedFd> {
        // `OwnedFd::try_clone` duplicates with F_DUPFD_CLOEXEC.
        self.pidfd.try_clone()
    }

    pub(super) fn pidfd(&self) -> RawFd {
        self.pidfd.as_raw_fd()
    }

    pub(super) fn send(&self, signal: libc::c_int) -> std::io::Result<()> {
        // SAFETY: pidfd_send_signal(2) with a null siginfo and no flags
        // only reads the fd, which `self` keeps open for the call.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0_u32,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Send `signal` to this process, retaining the syscall error for callers
    /// that need to report why the signal failed.
    pub fn try_signal(&self, signal: Signal) -> std::io::Result<()> {
        self.send(signal_number(signal))
    }

    /// Send `signal` to this process. False once it has been reaped.
    ///
    /// The pane teardown escalation path consumes a boolean to choose whether
    /// to advance; callers that log a failure use [`Self::try_signal`] so the
    /// kernel error is captured at the syscall boundary.
    pub fn signal(&self, signal: Signal) -> bool {
        self.try_signal(signal).is_ok()
    }

    /// Whether the process still holds its pid: running, or a zombie nobody
    /// has reaped yet. While this is true the pid cannot be reused, and
    /// neither can a process-group or session id equal to it.
    pub fn is_unreaped(&self) -> bool {
        // SAFETY: pidfd_send_signal(2) with signal zero and a null
        // siginfo only probes the process identified by this live fd.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                0,
                std::ptr::null::<libc::siginfo_t>(),
                0_u32,
            )
        };
        if result == 0 {
            return true;
        }
        // EPERM means the pidfd still names a live process that this
        // uid cannot signal. Only ESRCH proves the process was reaped.
        pidfd_probe_error_means_unreaped(&std::io::Error::last_os_error())
    }

    /// Whether the process has exited. A zombie counts as exited.
    pub fn has_exited(&self) -> bool {
        let mut descriptor = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd that lives on this stack frame; zero timeout.
        let ready = unsafe { libc::poll(&mut descriptor, 1, Wait::Now.poll_millis()) };
        ready > 0 && descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0
    }
}

fn pidfd_probe_error_means_unreaped(error: &std::io::Error) -> bool {
    error.raw_os_error() != Some(libc::ESRCH)
}

/// Wait until every handle's process has exited, or `timeout` passes.
/// Returns whether they all exited.
pub fn wait_for_process_exits(handles: &[&ProcessHandle], timeout: Duration) -> bool {
    // clock-io-ok: the public entry point supplies the real clock.
    wait_for_process_exits_with_clock(handles, Instant::now() + timeout, &Instant::now)
}

fn wait_for_process_exits_with_clock(
    handles: &[&ProcessHandle],
    deadline: Instant,
    now: &dyn Fn() -> Instant,
) -> bool {
    loop {
        let pending: Vec<&ProcessHandle> = handles
            .iter()
            .copied()
            .filter(|handle| !handle.has_exited())
            .collect();
        if pending.is_empty() {
            return true;
        }
        let Some(remaining) = remaining_until(deadline, now()) else {
            return false;
        };
        let mut descriptors: Vec<libc::pollfd> = pending
            .iter()
            .map(|handle| libc::pollfd {
                fd: handle.pidfd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let count = libc::nfds_t::try_from(descriptors.len()).unwrap_or(libc::nfds_t::MAX);
        // SAFETY: `descriptors` holds `count` initialised pollfds and outlives
        // the call; the fds are kept open by `handles`.
        let ready = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                count,
                Wait::from(remaining).poll_millis(),
            )
        };
        if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            crate::structured_log!(ERROR, event = process.pidfd_poll, outcome = "error", error = %std::io::Error::last_os_error(), "could not poll process pidfds");
            return false;
        }
    }
}

/// Every live process of session `session_id` other than its leader, each
/// held through a [`ProcessHandle`] so later signals cannot reach a reused
/// pid.
///
/// Membership is read straight from `/proc/<pid>/stat` for every process;
/// the leader's own stat is never consulted, so this works after the leader
/// has exited and been reaped. A candidate's stat is re-read after its handle
/// is open: if the handle is still unreaped afterwards, the pid was not
/// handed to anyone else in between and the stat described that process.
///
/// A session id is the leader's pid, and the kernel only reuses a number
/// nobody holds as a pid, process-group id or session id. So while the
/// leader is unreaped (`leader_reaped()` false) every process with this
/// session id is ours. Once it is reaped, a task that now holds pid
/// `session_id` proves the number was reused, which also proves the session
/// had no members left when that happened; nothing is returned then.
/// Remaining gap: the number is reused, the new owner starts its own session
/// and then exits and is reaped while its session lives on, all before this
/// runs. That needs a full pid wraparound between the pane's leader dying
/// and its teardown.
pub fn session_members(wanted: SessionId, leader_reaped: impl Fn() -> bool) -> Vec<ProcessHandle> {
    let session_leader = wanted.leader_pid();
    let mut handles = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = numeric_file_name(&entry) else {
            continue;
        };
        if pid == session_leader || process_session_id(pid) != Some(wanted) {
            continue;
        }
        let Some(handle) = ProcessHandle::open(pid) else {
            continue;
        };
        if process_session_id(pid) == Some(wanted) && handle.is_unreaped() {
            handles.push(handle);
        }
    }
    // A stat failure other than absence cannot rule out reuse, so it is read
    // as "reused" and nothing is returned: the members are left alone rather
    // than risk signalling another session's processes.
    if leader_reaped()
        && Path::new(&format!("/proc/{session_leader}"))
            .try_exists()
            .unwrap_or(true)
    {
        return Vec::new();
    }
    handles
}

pub(crate) fn numeric_file_name(entry: &std::fs::DirEntry) -> Option<Pid> {
    let file_name = entry.file_name();
    let value = file_name.to_str()?;
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().and_then(Pid::new)
}

fn process_session_id(pid: Pid) -> Option<SessionId> {
    ProcStat::read(pid).ok().map(|stat| stat.session)
}

/// Reap the exited child behind `pidfd` with `waitid(P_PIDFD, WEXITED)` and
/// return the status `Child::wait` would have reported. Call it once the
/// pidfd is readable: it blocks until the child exits.
pub fn reap_pidfd(pidfd: std::os::fd::BorrowedFd<'_>) -> std::io::Result<std::process::ExitStatus> {
    let id = libc::id_t::try_from(pidfd.as_raw_fd()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "pidfd cannot be represented as a waitid id",
        )
    })?;
    // SAFETY: siginfo_t is a plain C output record with a valid all-zero
    // representation; waitid overwrites its status fields on success.
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    loop {
        // SAFETY: `info` is writable storage for one siginfo_t and `id` is a
        // live pidfd borrowed for this call; WEXITED reaps that process.
        let result = unsafe { libc::waitid(libc::P_PIDFD, id, &mut info, libc::WEXITED) };
        if result == 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    // SAFETY: waitid succeeded with WEXITED, so it filled a SIGCHLD record
    // whose si_status field is initialized.
    let status = unsafe { info.si_status() };
    exit_status_from_waitid(info.si_code, status).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "waitid returned unexpected child status code {}",
                info.si_code
            ),
        )
    })
}

/// The `wait(2)` status word for a `waitid` result, so the `ExitStatus` is
/// the one `waitpid` (and so `Child::wait`) produces: exit code in bits 8-15,
/// terminating signal in bits 0-6, 0x80 for a core dump.
fn exit_status_from_waitid(
    code: libc::c_int,
    status: libc::c_int,
) -> Option<std::process::ExitStatus> {
    use std::os::unix::process::ExitStatusExt;

    let raw = match code {
        libc::CLD_EXITED => (status & 0xff) << 8,
        libc::CLD_KILLED => status & 0x7f,
        libc::CLD_DUMPED => (status & 0x7f) | 0x80,
        _ => return None,
    };
    Some(std::process::ExitStatus::from_raw(raw))
}

#[cfg(test)]
pub(super) fn session_and_tty_from_stat(stat: &str) -> Option<(i32, i32)> {
    ProcStat::parse(stat).map(|stat| (stat.session.as_pid_t(), stat.tty_nr))
}

#[cfg(test)]
pub(super) fn process_exists(pid: Pid) -> bool {
    // SAFETY: kill(2) with signal 0 only probes for the pid.
    let result = unsafe { libc::kill(pid.as_pid_t(), 0) };
    if result == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(test)]
mod reap_tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::os::unix::process::ExitStatusExt;

    use shepr_test_support::fixture::{self, Step};

    fn reap(step: Step) -> (std::process::ExitStatus, std::process::Child) {
        let child = fixture::command(&[step])
            .spawn()
            .expect("test precondition");
        let handle =
            ProcessHandle::open(Pid::new(child.id()).expect("child pid")).expect("child is alive");
        let pidfd = handle.try_clone_pidfd().expect("pidfd duplicates");
        (reap_pidfd(pidfd.as_fd()).expect("waitid reaps"), child)
    }

    #[test]
    fn reaped_status_matches_what_wait_reports() {
        let (status, mut child) = reap(Step::Exit(7));
        assert_eq!(status.code(), Some(7));
        assert_eq!(status.signal(), None);
        // Already reaped: a second wait finds no child, so none is left a zombie.
        assert!(child.try_wait().is_err());

        let (status, _child) = reap(Step::Raise(fixture::Signal::Kill));
        assert_eq!(status.code(), None);
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(!status.core_dumped());
    }

    #[test]
    fn waitid_codes_map_to_wait_status_words() {
        let exited = exit_status_from_waitid(libc::CLD_EXITED, 255).expect("exit");
        assert_eq!(exited.code(), Some(255));
        let killed = exit_status_from_waitid(libc::CLD_KILLED, libc::SIGTERM).expect("kill");
        assert_eq!(killed.signal(), Some(libc::SIGTERM));
        assert!(!killed.core_dumped());
        let dumped = exit_status_from_waitid(libc::CLD_DUMPED, libc::SIGSEGV).expect("dump");
        assert_eq!(dumped.signal(), Some(libc::SIGSEGV));
        assert!(dumped.core_dumped());
        assert_eq!(dumped.into_raw(), libc::SIGSEGV | 0x80);
        assert!(exit_status_from_waitid(libc::CLD_STOPPED, libc::SIGSTOP).is_none());
    }

    #[test]
    fn process_exit_wait_ends_when_the_injected_clock_reaches_the_deadline() {
        let mut child = fixture::command(&[Step::Sleep(Duration::from_secs(30))])
            .spawn()
            .expect("test precondition");
        let handle =
            ProcessHandle::open(Pid::new(child.id()).expect("child pid")).expect("child is alive");
        // The deadline is as far off as the child's sleep, but the injected
        // clock starts 20 ms short of it and reaches it on the next read.
        let started = Instant::now();
        let deadline = started + Duration::from_secs(30);
        let step = Duration::from_millis(20);
        let reads = std::cell::Cell::new(0_u32);
        let now = || {
            let read = reads.get();
            reads.set(read + 1);
            deadline - step + step * read
        };

        let exited = wait_for_process_exits_with_clock(&[&handle], deadline, &now);
        let elapsed = started.elapsed();
        // Reap the child before asserting, so a failure does not leave it
        // holding the test's output open for its whole sleep.
        child.kill().expect("test precondition");
        child.wait().expect("test precondition");

        assert!(!exited);
        assert_eq!(reads.get(), 2, "the wait ends on the read at the deadline");
        // Only a wait that ignored the injected clock sits out the sleep.
        assert!(
            elapsed < Duration::from_secs(5),
            "the wait took {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod stat_tests {
    use super::*;

    fn record(state: &str) -> String {
        // Fields 9-21 differ from starttime to catch off-by-one offsets.
        format!(
            "123 (name with ) (parens)) {state} 0 456 789 34817 -1 {} 9001 42",
            ["9"; 13].join(" ")
        )
    }

    #[test]
    fn parses_shared_fields_and_command_parentheses() {
        let stat = ProcStat::parse(&record("S")).expect("valid record");
        assert_eq!(stat.pid.get(), 123);
        assert_eq!(stat.comm, "name with ) (parens)");
        assert_eq!(stat.state, ProcState::Sleeping);
        assert_eq!(stat.parent, None);
        assert_eq!(stat.process_group.get(), 456);
        assert_eq!(stat.session.get(), 789);
        assert_eq!(stat.tty_nr, 34817);
        assert_eq!(stat.foreground_group, None);
        assert_eq!(stat.start_ticks, 9001);
        let foreground = record("R").replace(" -1 ", " 456 ");
        assert_eq!(
            ProcStat::parse(&foreground)
                .expect("valid record")
                .foreground_group,
            Pgid::new(456)
        );
    }

    #[test]
    fn rejects_invalid_ids_states_and_truncated_fields() {
        assert!(Pid::new(0).is_none());
        assert!(Pid::new(u32::MAX).is_none());
        assert_eq!(
            Pid::new(i32::MAX as u32).expect("maximum pid").as_pid_t(),
            i32::MAX
        );
        assert!(ProcStat::parse(&record("SS")).is_none());
        assert!(ProcStat::parse(&record("?")).is_none());
        assert!(ProcStat::parse(&record("S").replacen("123", "0", 1)).is_none());
        assert!(ProcStat::parse("123 (short) S 0 456 789 0 -1").is_none());
    }

    #[test]
    fn finished_and_remote_memory_predicates_agree_for_both_dead_codes() {
        for code in ['Z', 'X', 'x'] {
            let state = ProcState::from_code(code).expect("known state");
            assert!(state.is_finished());
            assert!(!state.allows_remote_memory_read());
        }
        assert!(!ProcState::Uninterruptible.is_finished());
        assert!(!ProcState::Uninterruptible.allows_remote_memory_read());
        assert!(ProcState::Stopped.is_stopped());
        assert!(ProcState::TracingStop.is_stopped());
        assert!(ProcState::Sleeping.allows_remote_memory_read());
    }

    #[test]
    fn current_record_matches_requested_process() {
        let pid = Pid::new(std::process::id()).expect("current pid");
        assert_eq!(ProcStat::read(pid).expect("current stat").pid, pid);
    }
}
