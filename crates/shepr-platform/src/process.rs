use super::*;
use std::{
    os::fd::{AsRawFd, RawFd},
    path::Path,
    time::{Duration, Instant},
};

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

/// How often exits are rechecked for handles that have no pidfd to poll.
const START_TIME_EXIT_RECHECK: Duration = Duration::from_millis(10);

/// A handle on one specific process. A signal sent through it reaches that
/// process or nobody; a plain `kill(pid)` can land on an unrelated process
/// the kernel later gave the same pid.
#[derive(Debug)]
pub struct ProcessHandle {
    pid: u32,
    identity: ProcessIdentity,
}

#[derive(Debug)]
enum ProcessIdentity {
    /// A pidfd: signals go through the kernel's own handle on the process.
    Pidfd(std::os::fd::OwnedFd),
    /// The fallback when no pidfd could be opened (a kernel before 5.3, or
    /// fd exhaustion): the process's start time, in clock ticks since boot,
    /// from `/proc/<pid>/stat`. A pid and a start time name one process; a
    /// reused pid belongs to a process that started later. The identity is
    /// rechecked right before each `kill(2)`. What that leaves open is the
    /// process exiting, being reaped and its pid going round the whole pid
    /// space to a new process between the recheck and the kill, a window of
    /// a few syscalls.
    StartTime(u64),
}

impl ProcessHandle {
    /// Open a handle on the process that holds `pid` right now. `None` when
    /// no such process exists.
    pub fn open(pid: u32) -> Option<Self> {
        use std::os::fd::FromRawFd;

        let raw_pid = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0)?;
        // SAFETY: pidfd_open(2) takes a pid and a flags word and returns a new
        // close-on-exec fd or -1; it reads and writes no memory of ours.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, raw_pid, 0_u32) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                // No such process, or `pid` names a thread, not a process.
                Some(libc::ESRCH | libc::EINVAL) => None,
                _ => {
                    tracing::debug!(pid, %error, "no pidfd; identifying the process by start time");
                    Self::open_by_start_time(pid)
                }
            };
        }
        let fd = RawFd::try_from(fd).ok()?;
        // SAFETY: `fd` was just returned by pidfd_open and nothing else owns it.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        Some(Self {
            pid,
            identity: ProcessIdentity::Pidfd(fd),
        })
    }

    /// A handle without a pidfd, identifying the process by its start time.
    pub(super) fn open_by_start_time(pid: u32) -> Option<Self> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, start_time) = state_and_start_time_from_stat(&stat)?;
        Some(Self {
            pid,
            identity: ProcessIdentity::StartTime(start_time),
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Duplicate the pidfd for readiness polling, if this handle has one.
    /// The returned descriptor has its own lifetime and can be registered
    /// with an async poller without exposing or transferring this handle's fd.
    pub fn try_clone_pidfd(&self) -> std::io::Result<Option<std::os::fd::OwnedFd>> {
        let ProcessIdentity::Pidfd(fd) = &self.identity else {
            return Ok(None);
        };
        // `OwnedFd::try_clone` duplicates with F_DUPFD_CLOEXEC.
        fd.try_clone().map(Some)
    }

    pub(super) fn pidfd(&self) -> Option<RawFd> {
        match &self.identity {
            ProcessIdentity::Pidfd(fd) => Some(fd.as_raw_fd()),
            ProcessIdentity::StartTime(_) => None,
        }
    }

    /// For a start-time handle: this process's state letter while it still
    /// holds its pid (a zombie included), `None` once it has been reaped.
    pub(super) fn state_by_start_time(&self, start_time: u64) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.pid)).ok()?;
        let (state, current) = state_and_start_time_from_stat(&stat)?;
        (current == start_time).then_some(state)
    }

    pub(super) fn send(&self, signal: libc::c_int) -> bool {
        match &self.identity {
            ProcessIdentity::Pidfd(fd) => {
                // SAFETY: pidfd_send_signal(2) with a null siginfo and no flags
                // only reads the fd, which `self` keeps open for the call.
                let result = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        signal,
                        std::ptr::null::<libc::siginfo_t>(),
                        0_u32,
                    )
                };
                result == 0
            }
            ProcessIdentity::StartTime(start_time) => {
                if self.state_by_start_time(*start_time).is_none() {
                    return false;
                }
                let Ok(pid) = libc::pid_t::try_from(self.pid) else {
                    return false;
                };
                // SAFETY: kill(2) touches no memory of this process. The pid
                // was just confirmed to still be this process; see
                // `ProcessIdentity::StartTime` for the window left.
                unsafe { libc::kill(pid, signal) == 0 }
            }
        }
    }

    /// Send `signal` to this process. False once it has been reaped.
    pub fn signal(&self, signal: Signal) -> bool {
        self.send(signal_number(signal))
    }

    /// Whether the process still holds its pid: running, or a zombie nobody
    /// has reaped yet. While this is true the pid cannot be reused, and
    /// neither can a process-group or session id equal to it.
    pub fn is_unreaped(&self) -> bool {
        match &self.identity {
            ProcessIdentity::Pidfd(fd) => {
                // SAFETY: pidfd_send_signal(2) with signal zero and a null
                // siginfo only probes the process identified by this live fd.
                let result = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
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
            ProcessIdentity::StartTime(start_time) => {
                self.state_by_start_time(*start_time).is_some()
            }
        }
    }

    /// Whether the process has exited. A zombie counts as exited.
    pub fn has_exited(&self) -> bool {
        match &self.identity {
            ProcessIdentity::Pidfd(fd) => {
                let mut descriptor = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one pollfd that lives on this stack frame; zero timeout.
                let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
                ready > 0 && descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0
            }
            ProcessIdentity::StartTime(start_time) => !matches!(
                self.state_by_start_time(*start_time),
                Some(state) if !matches!(state, 'Z' | 'X' | 'x')
            ),
        }
    }
}

fn pidfd_probe_error_means_unreaped(error: &std::io::Error) -> bool {
    error.raw_os_error() != Some(libc::ESRCH)
}

/// The state letter and start time (clock ticks since boot) from a
/// `/proc/<pid>/stat` line. The command name is skipped by its last `)`.
pub(super) fn state_and_start_time_from_stat(stat: &str) -> Option<(char, u64)> {
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    // starttime is field 22 of stat(5); `state` was field 3.
    let start_time = fields.nth(18)?.parse().ok()?;
    Some((state, start_time))
}

/// Wait until every handle's process has exited, or `timeout` passes.
/// Returns whether they all exited.
pub fn wait_for_process_exits(handles: &[&ProcessHandle], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let pending: Vec<&ProcessHandle> = handles
            .iter()
            .copied()
            .filter(|handle| !handle.has_exited())
            .collect();
        if pending.is_empty() {
            return true;
        }
        let Some(mut wait_ms) = poll_timeout_until(deadline) else {
            return false;
        };
        let mut descriptors: Vec<libc::pollfd> = pending
            .iter()
            .filter_map(|handle| handle.pidfd())
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        if descriptors.len() < pending.len() {
            // Handles without a pidfd have nothing to poll: recheck them soon.
            let recheck_ms = i32::try_from(START_TIME_EXIT_RECHECK.as_millis()).unwrap_or(10);
            wait_ms = wait_ms.min(recheck_ms);
        }
        let count = libc::nfds_t::try_from(descriptors.len()).unwrap_or(libc::nfds_t::MAX);
        // SAFETY: `descriptors` holds `count` initialised pollfds and outlives
        // the call (with none, poll only sleeps); the fds are kept open by
        // `handles`.
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), count, wait_ms) };
        if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            // A failing poll must not turn this into a busy loop.
            std::thread::sleep(START_TIME_EXIT_RECHECK);
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
pub fn session_member_handles(
    session_id: u32,
    leader_reaped: impl Fn() -> bool,
) -> Vec<ProcessHandle> {
    session_member_handles_with(session_id, leader_reaped, ProcessHandle::open)
}

pub(super) fn session_member_handles_with(
    session_id: u32,
    leader_reaped: impl Fn() -> bool,
    open: impl Fn(u32) -> Option<ProcessHandle>,
) -> Vec<ProcessHandle> {
    let Ok(wanted) = i32::try_from(session_id) else {
        return Vec::new();
    };
    if wanted <= 0 {
        return Vec::new();
    }
    let mut handles = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = numeric_file_name(&entry) else {
            continue;
        };
        if pid == session_id || process_session_id(pid) != Some(wanted) {
            continue;
        }
        let Some(handle) = open(pid) else {
            continue;
        };
        if process_session_id(pid) == Some(wanted) && handle.is_unreaped() {
            handles.push(handle);
        }
    }
    if leader_reaped() && Path::new(&format!("/proc/{session_id}")).exists() {
        return Vec::new();
    }
    handles
}

fn numeric_file_name(entry: &std::fs::DirEntry) -> Option<u32> {
    let file_name = entry.file_name();
    let value = file_name.to_str()?;
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn process_session_id(pid: u32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    session_and_tty_from_stat(&stat).map(|(session, _tty)| session)
}

/// The session id and controlling-terminal device (`tty_nr`, 0 for none) from
/// a `/proc/<pid>/stat` line. The command name is skipped by its last `)`, as
/// it may itself contain spaces and parentheses.
pub(super) fn session_and_tty_from_stat(stat: &str) -> Option<(i32, i32)> {
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let mut fields = rest.split_whitespace().skip(3);
    let session = fields.next()?.parse().ok()?;
    let tty_nr = fields.next()?.parse().ok()?;
    Some((session, tty_nr))
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

/// Signal processes by bare pid. Test-only: production code signals through
/// `ProcessHandle`, which cannot hit a reused pid.
#[cfg(any(test, feature = "test-support"))]
pub fn signal_processes(pids: &[u32], signal: Signal) {
    for &pid in pids {
        let Ok(pid) = i32::try_from(pid) else {
            continue;
        };
        if pid <= 0 {
            continue;
        }
        // SAFETY: kill(2) touches no memory of this process.
        unsafe {
            libc::kill(pid, signal_number(signal));
        }
    }
}

#[cfg(test)]
pub(super) fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: kill(2) with signal 0 only probes for the pid.
    let result = unsafe { libc::kill(pid, 0) };
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
        let handle = ProcessHandle::open(child.id()).expect("child is alive");
        let pidfd = handle
            .try_clone_pidfd()
            .expect("pidfd duplicates")
            .expect("kernel supports pidfds");
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
}
