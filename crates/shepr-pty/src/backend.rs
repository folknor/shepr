//! PTY allocation and child launch.
//!
//! Shepr owns this directly on libc rather than delegating to
//! `alacritty_terminal::tty`: that module cannot remove inherited environment
//! variables or set a login-shell argv0, injects its own environment
//! (`ALACRITTY_WINDOW_ID`, `WINDOWID`), keeps the `Child` behind a shared
//! reference with a blocking SIGHUP-and-wait `Drop`, registers a SIGCHLD
//! handler per PTY, and exits the process when a resize ioctl fails. Shepr's
//! PTY actor (`crate::actor`) owns the master fd, the IO loop, and
//! resizing; this module only opens the PTY and starts the child.
//!
//! Nor does it use `std::process::Command`: its `spawn` waits until the child
//! has changed directory and exec'd, and either can block for as long as a hung
//! mount does. A pane spawn runs on the server's event loop, so the fork here
//! returns at once with the child's pid. The child does the chdir and the exec
//! on its own and reports how they went over its status channel
//! (`crate::launch`). Nothing on the parent side touches the user's
//! filesystem.

use std::ffi::c_char;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::ExitStatus;

use crate::command::{LaunchSpec, PtyCommand};
use crate::fd;
use crate::launch::{self, Registration, StatusDelivery};
use crate::limits::{GETDENTS_READ_BUFFER_BYTES, LAUNCH_STATUS_RECORD_BYTES, MAX_SIGNAL_NUMBER};

// limits-exempt: process exit status protocol, as shells report a failed setup.
const EXIT_SETUP_FAILED: libc::c_int = 126;
// limits-exempt: process exit status protocol, as shells report a failed command.
const EXIT_LAUNCH_FAILED: libc::c_int = 127;

unsafe extern "C" {
    /// glibc's and musl's fork without the `pthread_atfork` handlers, whose
    /// code and locks the child of a multithreaded process must not run.
    fn _Fork() -> libc::pid_t;
}

/// Both ends of a freshly opened PTY. Both fds are close-on-exec.
pub(crate) struct OpenedPty {
    pub master: OwnedFd,
    pub slave: OwnedFd,
}

/// A launching child and the parent's only handle on its PTY: the master fd.
pub struct SpawnedPty {
    pub master_fd: OwnedFd,
    pub child: PaneChild,
    /// The directories the child tries, in order; its chdir status record names
    /// a candidate by index.
    pub cwd_candidates: Vec<std::path::PathBuf>,
    /// The launch's claim on the child's status channel.
    pub status: Registration,
}

/// The pane child, forked by shepr rather than std: its stable handle and exit
/// status once reaped. Dropping it neither kills nor reaps the child.
#[derive(Debug)]
pub struct PaneChild {
    handle: std::sync::Arc<shepr_platform::ProcessHandle>,
    status: Option<ExitStatus>,
}

impl PaneChild {
    pub fn id(&self) -> u32 {
        self.handle.pid()
    }

    pub fn process_id(&self) -> shepr_platform::Pid {
        self.handle.process_id()
    }

    fn raw_pid(&self) -> libc::pid_t {
        self.process_id().as_pid_t()
    }

    /// SIGKILL to the child, unless it has already been reaped here (its pid
    /// may belong to another process by then).
    pub fn kill(&mut self) -> io::Result<()> {
        if self.status.is_some() {
            return Ok(());
        }
        if !self.handle.signal(shepr_platform::Signal::Kill) {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Blocks until the child exits, and reaps it.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.wait_with(0)?
            .ok_or_else(|| io::Error::other("a blocking wait returned no status"))
    }

    /// Reaps the child if it has exited.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait_with(libc::WNOHANG)
    }

    /// The same stable identity used by waiting, signalling and observation.
    pub fn handle(&self) -> std::sync::Arc<shepr_platform::ProcessHandle> {
        std::sync::Arc::clone(&self.handle)
    }

    /// Reap through the child's pidfd after readiness was observed.
    pub fn wait_pidfd(&mut self) -> io::Result<ExitStatus> {
        use std::os::fd::AsFd;
        if let Some(status) = self.status {
            return Ok(status);
        }
        let pidfd = self.handle.try_clone_pidfd()?;
        let status = shepr_platform::reap_pidfd(pidfd.as_fd())?;
        self.status = Some(status);
        Ok(status)
    }

    fn wait_with(&mut self, flags: libc::c_int) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let pid = self.raw_pid();
        let mut raw = 0;
        loop {
            // SAFETY: `raw` is a live writable int; waitpid(2) reaps only this
            // child of ours.
            let result = unsafe { libc::waitpid(pid, &mut raw, flags) };
            if result == 0 {
                return Ok(None);
            }
            if result > 0 {
                use std::os::unix::process::ExitStatusExt;
                let status = ExitStatus::from_raw(raw);
                self.status = Some(status);
                return Ok(Some(status));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

/// Open a PTY pair whose first `TIOCSWINSZ` carries `geometry`, pixel
/// dimensions included, so a child that reads its window size once at startup
/// sees the pixel size it will keep.
pub(crate) fn open_pty_with_geometry(
    geometry: shepr_core::geometry::PaneGeometry,
) -> io::Result<OpenedPty> {
    // Linux accepts O_CLOEXEC while opening /dev/ptmx, closing the race with
    // unrelated concurrent process spawns before either PTY fd is wrapped.
    const PTMX: &[u8] = b"/dev/ptmx\0";
    // SAFETY: `PTMX` is NUL-terminated and remains valid for the call; open
    // reads it without retaining the pointer and takes no mode argument here.
    let master = unsafe {
        libc::open(
            PTMX.as_ptr().cast(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if master < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: open succeeded, so `master` is a fresh fd nothing else owns.
    let master = unsafe { OwnedFd::from_raw_fd(master) };

    // SAFETY: grantpt and unlockpt take a live PTY master fd and retain no
    // references to it.
    if unsafe { libc::grantpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::unlockpt(master.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }

    // Open the slave through the master (`TIOCGPTPEER`, Linux 4.13+), as
    // glibc's openpty does, rather than by ptsname path: the peer is the
    // master's own devpts instance even when /dev/pts in this mount namespace
    // is another one, and `O_CLOEXEC` is set at creation so no spawn can
    // inherit it.
    // SAFETY: `TIOCGPTPEER` takes the open flags as its integer argument and
    // returns a new fd; nothing is read from or written to our memory.
    let slave = unsafe {
        libc::ioctl(
            master.as_raw_fd(),
            libc::TIOCGPTPEER,
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if slave < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the ioctl succeeded, so `slave` is a fresh fd nothing else owns.
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };

    fd::resize_pty_fd(master.as_raw_fd(), geometry)?;
    enable_utf8_input(&master);
    Ok(OpenedPty { master, slave })
}

/// Mark the line discipline as UTF-8 so canonical-mode erase removes whole
/// characters. Best effort, as in alacritty's tty setup: the pane still works
/// without it, only canonical-mode erase of multibyte characters degrades, so
/// a failure is logged rather than failing the spawn.
fn enable_utf8_input(master: &OwnedFd) {
    // SAFETY: termios is a plain C struct of integers and arrays, for which
    // all-zero bytes are a valid value; tcgetattr overwrites it below.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `master` is an open fd borrowed for the call, and `termios` is a
    // live, writable termios that tcgetattr fills in and does not retain.
    if unsafe { libc::tcgetattr(master.as_raw_fd(), &mut termios) } != 0 {
        let err = io::Error::last_os_error();
        tracing::warn!(error = %err, "could not read PTY attributes to enable UTF-8 input");
        return;
    }
    termios.c_iflag |= libc::IUTF8;
    // SAFETY: as above; tcsetattr only reads `termios`.
    if unsafe { libc::tcsetattr(master.as_raw_fd(), libc::TCSANOW, &termios) } != 0 {
        let err = io::Error::last_os_error();
        tracing::warn!(error = %err, "could not enable UTF-8 input on the PTY");
    }
}

/// Open a PTY sized `geometry` and fork `cmd`'s child into it, without waiting
/// for the child's chdir or exec. `deliver` receives the child's status
/// channel once it connects. The parent's slave fd is closed before returning.
pub fn spawn_pty(
    geometry: shepr_core::geometry::PaneGeometry,
    cmd: &PtyCommand,
    deliver: StatusDelivery,
) -> io::Result<SpawnedPty> {
    let service = launch::service()?;
    let spec = cmd.launch_spec(service.passwd_home())?;
    let OpenedPty { master, slave } = open_pty_with_geometry(geometry)?;
    // dup2 onto 0-2 in the child must not start from one of those numbers:
    // dup2(fd, fd) would keep its close-on-exec flag.
    let slave = above_stdio(slave)?;
    let ticket = service.next_ticket();
    let (address, address_len) = service.address();
    let plan = ChildPlan::new(&spec, slave.as_raw_fd(), address, address_len, ticket);
    let pid = fork_child(&plan)?;
    drop(slave);
    // No watcher can reap this child yet, so pidfd_open names this fork.
    let Some(handle) = shepr_platform::ProcessHandle::open_process(pid) else {
        // SAFETY: this fork has never been handed to a waiter, so its pid
        // cannot have been reused. This is only the failed acquisition path.
        if unsafe { libc::kill(pid.as_pid_t(), libc::SIGKILL) } != 0 {
            tracing::warn!(pid = %pid, error = %io::Error::last_os_error(), "could not kill failed pane launch");
        }
        if let Err(error) = std::thread::Builder::new()
            .name("shepr-launch-reaper".into())
            .spawn(move || {
                let mut status = 0;
                loop {
                    // SAFETY: this is our unreaped child and status is writable.
                    let result = unsafe { libc::waitpid(pid.as_pid_t(), &mut status, 0) };
                    if result >= 0
                        || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                    {
                        break;
                    }
                }
            })
        {
            tracing::warn!(pid = %pid, %error, "could not start failed launch reaper");
        }
        return Err(io::Error::other("no process handle for the pane's child"));
    };
    // Registered right after the fork: a connection that arrives first waits
    // for it.
    let status = service.register(ticket, handle.process_id(), deliver);
    Ok(SpawnedPty {
        master_fd: master,
        child: PaneChild {
            handle: std::sync::Arc::new(handle),
            status: None,
        },
        cwd_candidates: spec
            .candidates
            .iter()
            .map(|candidate| std::path::PathBuf::from(&candidate.path))
            .collect(),
        status,
    })
}

fn above_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() > 2 {
        return Ok(fd);
    }
    // SAFETY: fcntl(F_DUPFD_CLOEXEC) takes the live fd and an integer floor
    // and returns a new fd or -1.
    let duplicate = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl succeeded, so `duplicate` is a fresh fd nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

/// Every pointer the child dereferences, built before the fork from `spec`,
/// which outlives it: the child of a multithreaded process may not allocate.
struct ChildPlan<'a> {
    slave: RawFd,
    program: *const c_char,
    argv: Vec<*const c_char>,
    dirs: Vec<*const c_char>,
    envps: Vec<Vec<*const c_char>>,
    address: &'a libc::sockaddr_un,
    address_len: libc::socklen_t,
    hello: [u8; LAUNCH_STATUS_RECORD_BYTES],
}

impl<'a> ChildPlan<'a> {
    fn new(
        spec: &'a LaunchSpec,
        slave: RawFd,
        address: &'a libc::sockaddr_un,
        address_len: libc::socklen_t,
        ticket: u64,
    ) -> Self {
        let terminated = |strings: &[std::ffi::CString]| {
            strings
                .iter()
                .map(|string| string.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect::<Vec<_>>()
        };
        Self {
            slave,
            program: spec.program.as_ptr(),
            argv: terminated(&spec.argv),
            dirs: spec
                .candidates
                .iter()
                .map(|candidate| candidate.dir.as_ptr())
                .collect(),
            envps: spec
                .candidates
                .iter()
                .map(|candidate| terminated(&candidate.envp))
                .collect(),
            address,
            address_len,
            hello: launch::hello_record(ticket),
        }
    }
}

/// Forks with every signal blocked on this thread, so no server handler can
/// run in the child before it resets them, and restores this thread's mask.
fn fork_child(plan: &ChildPlan<'_>) -> io::Result<shepr_platform::Pid> {
    // SAFETY: sigset_t is a plain bit array; all-zero is valid and sigfillset
    // sets it fully.
    let mut all: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `all` is a live writable sigset_t.
    unsafe { libc::sigfillset(&mut all) };
    // SAFETY: both sets are live locals; pthread_sigmask changes only this
    // thread's mask and returns an error number rather than setting errno.
    let blocked = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &all, &mut previous) };
    if blocked != 0 {
        return Err(io::Error::from_raw_os_error(blocked));
    }
    // SAFETY: _Fork has no preconditions. In the child, only `run_child`
    // runs, which makes async-signal-safe calls on memory prepared above.
    let pid = unsafe { _Fork() };
    if pid == 0 {
        // SAFETY: this is the freshly forked child, and `plan` points at
        // memory the fork copied.
        unsafe { run_child(plan) }
    }
    let fork_error = (pid < 0).then(io::Error::last_os_error);
    // SAFETY: `previous` is the mask saved above; the old-mask pointer is null.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) };
    if let Some(error) = fork_error {
        return Err(error);
    }
    u32::try_from(pid)
        .ok()
        .and_then(shepr_platform::Pid::new)
        .ok_or_else(|| io::Error::other("fork returned an invalid pid"))
}

fn errno() -> libc::c_int {
    // SAFETY: __errno_location returns this thread's errno slot, valid for
    // the thread's lifetime; reading it is async-signal-safe.
    unsafe { *libc::__errno_location() }
}

/// Exits the forked child without running any destructor or atexit handler.
fn child_exit(code: libc::c_int) -> ! {
    // SAFETY: _exit(2) ends this process at once and is async-signal-safe.
    unsafe { libc::_exit(code) }
}

fn send_record(channel: RawFd, record: &[u8; LAUNCH_STATUS_RECORD_BYTES]) -> bool {
    // SAFETY: `record` is a live stack array of the length passed; send(2)
    // is async-signal-safe, and MSG_NOSIGNAL keeps a closed peer from
    // raising SIGPIPE.
    let sent = unsafe {
        libc::send(
            channel,
            record.as_ptr().cast(),
            record.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    usize::try_from(sent).is_ok_and(|sent| sent == record.len())
}

/// The forked child. Every call is async-signal-safe and touches only this
/// process's own state: no allocation, no lock, no destructor, no panic.
///
/// Order matters. Dispositions are reset while every signal is still blocked
/// (the mask came from `fork_child`), so no server handler runs here. Every
/// inherited fd but the PTY slave is closed before any filesystem step: a
/// child stuck in chdir must not hold the server's lease, sockets or other
/// panes' PTYs. The status channel is connected before chdir, so a hung
/// chdir is still a launch in progress that the server can see.
///
/// # Safety
///
/// Call only in a freshly forked child, with `plan` built by `ChildPlan::new`.
unsafe fn run_child(plan: &ChildPlan<'_>) -> ! {
    // SAFETY: sigaction is a plain C struct; all-zero is valid, and the
    // fields that matter are set below.
    let mut default_action: libc::sigaction = unsafe { std::mem::zeroed() };
    default_action.sa_sigaction = libc::SIG_DFL;
    default_action.sa_flags = 0;
    // SAFETY: `default_action.sa_mask` is a live writable sigset_t.
    if unsafe { libc::sigemptyset(&mut default_action.sa_mask) } != 0 {
        child_exit(EXIT_SETUP_FAILED);
    }
    for signo in 1..=MAX_SIGNAL_NUMBER {
        if matches!(signo, libc::SIGKILL | libc::SIGSTOP) {
            continue;
        }
        // SAFETY: sigaction is async-signal-safe; the action pointer is live
        // and the old-action pointer null. Unsupported or libc-reserved
        // numbers report EINVAL and are skipped.
        if unsafe { libc::sigaction(signo, &default_action, std::ptr::null_mut()) } != 0
            && errno() != libc::EINVAL
        {
            child_exit(EXIT_SETUP_FAILED);
        }
    }
    for target in 0..=2 {
        // SAFETY: dup2(2) takes two fds; the slave is open in this child.
        if unsafe { libc::dup2(plan.slave, target) } < 0 {
            child_exit(EXIT_SETUP_FAILED);
        }
    }
    if !close_inherited_fds() {
        child_exit(EXIT_SETUP_FAILED);
    }
    // New session, then take the PTY (now on stdin) as the controlling
    // terminal so job control and SIGWINCH reach the child.
    // SAFETY: setsid(2) takes no arguments; TIOCSCTTY on fd 0 takes an integer.
    if unsafe { libc::setsid() } == -1 || unsafe { libc::ioctl(0, libc::TIOCSCTTY, 0) } == -1 {
        child_exit(EXIT_SETUP_FAILED);
    }
    // SAFETY: socket(2) takes integers and returns a new fd or -1.
    let status =
        unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if status < 0 {
        child_exit(EXIT_SETUP_FAILED);
    }
    // SAFETY: `plan.address` is the listener's sockaddr_un with its exact
    // length; connect(2) reads it during the call only.
    if unsafe {
        libc::connect(
            status,
            std::ptr::from_ref(plan.address).cast(),
            plan.address_len,
        )
    } != 0
        || !send_record(status, &plan.hello)
    {
        child_exit(EXIT_SETUP_FAILED);
    }
    // SAFETY: sigset_t is a plain bit array; sigemptyset sets it.
    let mut empty: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `empty` is a live sigset_t; the old-mask pointer is null.
    if unsafe { libc::sigemptyset(&mut empty) } != 0
        || unsafe { libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) } != 0
    {
        child_exit(EXIT_SETUP_FAILED);
    }
    let mut selected = None;
    // Only the first candidate's failure is reported: it is the directory the
    // pane was meant to open in, and the one the user can fix. The rest are
    // fallbacks tried because of it.
    let mut first_failure = None;
    for (index, dir) in plan.dirs.iter().enumerate() {
        // SAFETY: `dir` points at a NUL-terminated string the fork copied.
        if unsafe { libc::chdir(*dir) } == 0 {
            selected = Some(index);
            break;
        }
        if first_failure.is_none() {
            first_failure = Some(errno());
        }
    }
    let Some(index) = selected else {
        if let Some(errno) = first_failure {
            send_record(status, &launch::chdir_failed_record(errno));
        }
        child_exit(EXIT_LAUNCH_FAILED);
    };
    send_record(
        status,
        &launch::chdir_ok_record(u32::try_from(index).unwrap_or(u32::MAX)),
    );
    // SAFETY: program, argv and the selected envp are NUL-terminated strings
    // and null-terminated pointer arrays the fork copied. execve only returns
    // on failure.
    unsafe { libc::execve(plan.program, plan.argv.as_ptr(), plan.envps[index].as_ptr()) };
    send_record(status, &launch::exec_failed_record(errno()));
    child_exit(EXIT_LAUNCH_FAILED)
}

/// Closes every fd from 3 up in the forked child. Kernels before 5.9 lack
/// close_range; the fallback walks procfs with raw syscalls and a stack
/// buffer so it stays async-signal-safe.
fn close_inherited_fds() -> bool {
    let first_fd: libc::c_uint = 3;
    // SAFETY: close_range(2) takes integers and affects only this child's
    // descriptor table.
    if unsafe { libc::syscall(libc::SYS_close_range, first_fd, libc::c_uint::MAX, 0) } == 0 {
        return true;
    }

    const PROC_SELF_FD: &[u8] = b"/proc/self/fd\0";
    // SAFETY: `PROC_SELF_FD` is a live NUL-terminated path; open(2) reads it
    // during the call only.
    let directory_fd = unsafe {
        libc::open(
            PROC_SELF_FD.as_ptr().cast(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if directory_fd < 0 {
        return false;
    }

    // limits-exempt: a field offset in the Linux `linux_dirent64` record.
    const DIRENT_RECLEN_OFFSET: usize = 16;
    // limits-exempt: a field offset in the Linux `linux_dirent64` record.
    const DIRENT_NAME_OFFSET: usize = 19;
    let mut buffer = [0u8; GETDENTS_READ_BUFFER_BYTES];
    let closed_all = 'scan: loop {
        // SAFETY: getdents64 writes at most `buffer.len()` bytes into this
        // live stack buffer and reads only the integer directory fd.
        let bytes_read = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory_fd,
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        if bytes_read == 0 {
            break true;
        }
        if bytes_read < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            break false;
        }
        let Ok(bytes_read) = usize::try_from(bytes_read) else {
            break false;
        };
        if bytes_read > buffer.len() {
            break false;
        }
        let mut offset = 0;
        while offset < bytes_read {
            if bytes_read - offset < DIRENT_NAME_OFFSET {
                break 'scan false;
            }
            let reclen_offset = offset + DIRENT_RECLEN_OFFSET;
            let record_len = usize::from(u16::from_ne_bytes([
                buffer[reclen_offset],
                buffer[reclen_offset + 1],
            ]));
            if record_len <= DIRENT_NAME_OFFSET || offset + record_len > bytes_read {
                break 'scan false;
            }
            let name = &buffer[offset + DIRENT_NAME_OFFSET..offset + record_len];
            let Some(name_len) = name.iter().position(|byte| *byte == 0) else {
                break 'scan false;
            };
            let mut fd = 0i32;
            let mut valid_fd = name_len > 0;
            for byte in &name[..name_len] {
                if !byte.is_ascii_digit() {
                    valid_fd = false;
                    break;
                }
                let Some(next_fd) = fd
                    .checked_mul(10)
                    .and_then(|fd| fd.checked_add(i32::from(*byte - b'0')))
                else {
                    valid_fd = false;
                    break;
                };
                fd = next_fd;
            }
            if valid_fd && fd > 2 && fd != directory_fd {
                // SAFETY: close(2) takes an integer; an fd closed since the
                // directory snapshot just reports EBADF.
                unsafe { libc::close(fd) };
            }
            offset += record_len;
        }
    };
    // SAFETY: closes the procfs directory opened above.
    unsafe { libc::close(directory_fd) };
    closed_all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::{LaunchRecord, RecordRead};
    use shepr_test_support::fixture::{self, Step};
    use std::io::Read;
    use std::sync::{Mutex, OnceLock};

    fn fixture_command(steps: &[Step]) -> PtyCommand {
        let scratch = shepr_test_support::ScratchDir::new("pty-backend-fixture");
        let path = fixture::stand_in(scratch.path(), "shepr-fixture", steps);
        PtyCommand::interactive_shell(&fixture::resolved_shell(&path), false)
    }

    fn test_geometry() -> shepr_core::geometry::PaneGeometry {
        shepr_core::geometry::PaneGeometry::new(80, 24, 0, 0)
    }

    fn ignore_status() -> StatusDelivery {
        Box::new(drop)
    }

    /// Spawns `cmd` and collects every status record until the child's end
    /// closes.
    fn spawn_and_read_status(cmd: &PtyCommand) -> (SpawnedPty, Vec<LaunchRecord>) {
        let (sender, receiver) = std::sync::mpsc::channel();
        let spawned = spawn_pty(
            test_geometry(),
            cmd,
            Box::new(move |channel| {
                sender.send(channel).ok();
            }),
        )
        .expect("pty setup succeeds");
        let channel = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the child connects its status channel");
        let mut records = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match crate::launch::read_record(&channel).expect("read a status record") {
                RecordRead::Record(record) => records.push(record),
                RecordRead::Eof => break,
                RecordRead::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "status never closed");
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
        (spawned, records)
    }

    fn pty_fd_test_lock() -> &'static Mutex<()> {
        // PTY allocation changes /proc/self/fd, so every test that opens a
        // PTY uses this guard while process-wide fd counts are asserted.
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn parent_pty_fd_targets() -> Vec<String> {
        let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
            return Vec::new();
        };
        let mut targets: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .map(|target| target.to_string_lossy().into_owned())
            .filter(|target| target.starts_with("/dev/pts/") || target == "/dev/ptmx")
            .collect();
        targets.sort();
        targets
    }

    fn parent_pty_fd_count() -> usize {
        parent_pty_fd_targets().len()
    }

    #[test]
    fn pty_spawn_leaves_one_parent_pty_fd() {
        let _guard = crate::locks::lock_auxiliary(pty_fd_test_lock());
        let before = parent_pty_fd_count();
        let mut cmd = fixture_command(&[Step::Cat]);
        cmd.env(
            shepr_core::env::EnvVar::SheprEnv,
            shepr_core::env::SHEPR_ENV_IN_PANE,
        );

        let mut spawned =
            spawn_pty(test_geometry(), &cmd, ignore_status()).expect("pty setup succeeds");
        let after_spawn = parent_pty_fd_count();

        assert_eq!(
            after_spawn,
            before + 1,
            "pty setup should leave only the Shepr-owned master fd in the parent: {:?}",
            parent_pty_fd_targets()
        );

        spawned.child.kill().expect("kill the cat child");
        spawned.child.wait().expect("reap the cat child");
        drop(spawned.master_fd);
    }

    #[test]
    fn child_is_session_leader_with_pty_as_controlling_terminal() {
        let _guard = crate::locks::lock_auxiliary(pty_fd_test_lock());
        let cmd = fixture_command(&[Step::Sleep(std::time::Duration::from_secs(30))]);
        let mut spawned =
            spawn_pty(test_geometry(), &cmd, ignore_status()).expect("pty setup succeeds");
        let pid = libc::pid_t::try_from(spawned.child.id()).expect("pid fits pid_t");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let foreground = loop {
            // SAFETY: tcgetpgrp(3) on an fd `spawned` keeps open; no memory.
            let pgrp = unsafe { libc::tcgetpgrp(spawned.master_fd.as_raw_fd()) };
            if pgrp == pid || std::time::Instant::now() >= deadline {
                break pgrp;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        // SAFETY: getsid(2) takes a pid and touches no memory.
        let session = unsafe { libc::getsid(pid) };

        spawned.child.kill().expect("kill the sleeping child");
        spawned.child.wait().expect("reap the sleeping child");
        assert_eq!(session, pid, "child must lead its own session");
        assert_eq!(foreground, pid, "child must own the PTY foreground group");
    }

    #[test]
    fn child_output_reaches_master_and_exit_status_is_reported() {
        let _guard = crate::locks::lock_auxiliary(pty_fd_test_lock());
        let cmd = fixture_command(&[Step::Print("shepr-pty-ok".into()), Step::Exit(7)]);
        let mut spawned =
            spawn_pty(test_geometry(), &cmd, ignore_status()).expect("pty setup succeeds");
        let status = spawned.child.wait().expect("wait for child");
        assert_eq!(status.code(), Some(7));

        let mut master = std::fs::File::from(spawned.master_fd);
        let mut output = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => output.extend_from_slice(&buf[..read]),
                // EIO means the slave side is closed: all output was read.
                Err(err) if err.raw_os_error() == Some(libc::EIO) => break,
                Err(err) => panic!("read pty master: {err}"),
            }
        }
        assert!(
            String::from_utf8_lossy(&output).contains("shepr-pty-ok"),
            "unexpected pty output: {output:?}"
        );
    }

    #[test]
    fn first_window_size_carries_the_pixel_dimensions() {
        let _guard = crate::locks::lock_auxiliary(pty_fd_test_lock());
        let cmd = fixture_command(&[Step::Sleep(std::time::Duration::from_secs(30))]);
        let mut spawned = spawn_pty(
            shepr_core::geometry::PaneGeometry::new(100, 30, 9, 18),
            &cmd,
            ignore_status(),
        )
        .expect("pty setup succeeds");

        // SAFETY: zero is a valid initial byte representation for winsize, and
        // TIOCGWINSZ writes one winsize to this live local value.
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: the master fd is open for the whole call.
        let result =
            unsafe { libc::ioctl(spawned.master_fd.as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
        spawned.child.kill().expect("kill the sleeping child");
        spawned.child.wait().expect("reap the sleeping child");

        assert_eq!(result, 0, "TIOCGWINSZ succeeds");
        assert_eq!(
            (size.ws_row, size.ws_col, size.ws_xpixel, size.ws_ypixel),
            (30, 100, 900, 540)
        );
    }

    #[test]
    fn a_launched_shell_reports_its_cwd_then_closes_the_channel_at_exec() {
        let _guard = crate::locks::lock_auxiliary(pty_fd_test_lock());
        let scratch = shepr_test_support::ScratchDir::new("pty-launch-ok");
        let mut cmd = fixture_command(&[Step::Sleep(std::time::Duration::from_secs(30))]);
        cmd.cwd(scratch.path());
        let (mut spawned, records) = spawn_and_read_status(&cmd);
        assert_eq!(records, [LaunchRecord::ChdirOk(0)]);
        assert!(
            spawned.child.try_wait().expect("check the child").is_none(),
            "the channel closed at exec while the shell kept running"
        );
        assert_eq!(spawned.cwd_candidates[0], scratch.path());
        spawned.child.kill().expect("kill the shell");
        spawned.child.wait().expect("reap the shell");
    }

    #[test]
    fn a_missing_required_cwd_is_reported_without_a_fallback() {
        let _guard = crate::locks::lock_auxiliary(pty_fd_test_lock());
        let scratch = shepr_test_support::ScratchDir::new("pty-launch-missing-cwd");
        let mut cmd = fixture_command(&[Step::Exit(0)]);
        cmd.cwd(scratch.join("missing"));
        cmd.require_cwd();
        let (mut spawned, records) = spawn_and_read_status(&cmd);
        assert_eq!(records, [LaunchRecord::ChdirFailed(libc::ENOENT)]);
        spawned.child.wait().expect("reap the failed launch");
    }

    #[test]
    fn a_missing_cwd_falls_back_to_home() {
        let _guard = crate::locks::lock_auxiliary(pty_fd_test_lock());
        let scratch = shepr_test_support::ScratchDir::new("pty-launch-home");
        let mut cmd = fixture_command(&[Step::Sleep(std::time::Duration::from_secs(30))]);
        cmd.env("HOME", scratch.path());
        cmd.cwd(scratch.join("removed-before-spawn"));
        let (mut spawned, records) = spawn_and_read_status(&cmd);
        assert_eq!(records, [LaunchRecord::ChdirOk(1)]);
        assert_eq!(spawned.cwd_candidates[1], scratch.path());
        spawned.child.kill().expect("kill the shell");
        spawned.child.wait().expect("reap the shell");
    }

    #[test]
    fn a_missing_shell_is_reported_as_an_exec_failure() {
        let _guard = crate::locks::lock_auxiliary(pty_fd_test_lock());
        let cmd = PtyCommand::interactive_shell(
            &fixture::resolved_shell("/__shepr_missing_program__"),
            false,
        );
        let (mut spawned, records) = spawn_and_read_status(&cmd);
        assert!(matches!(records.as_slice(), [
            LaunchRecord::ChdirOk(_),
            LaunchRecord::ExecFailed(errno)
        ] if *errno == libc::ENOENT));
        spawned.child.wait().expect("reap the failed launch");
    }

    #[test]
    fn the_child_keeps_no_inherited_descriptor() {
        let _guard = crate::locks::lock_auxiliary(pty_fd_test_lock());
        // A server fd without close-on-exec must not reach the shell.
        // SAFETY: dup(2) of stdin returns a new fd or -1.
        let leaked = unsafe { libc::dup(0) };
        assert!(leaked > 2, "test precondition");
        let cmd = fixture_command(&[Step::Sleep(std::time::Duration::from_secs(30))]);
        let (mut spawned, _) = spawn_and_read_status(&cmd);
        let fds: Vec<String> = std::fs::read_dir(format!("/proc/{}/fd", spawned.child.id()))
            .expect("list child fds")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        spawned.child.kill().expect("kill the shell");
        spawned.child.wait().expect("reap the shell");
        // SAFETY: closes the fd duplicated above.
        unsafe { libc::close(leaked) };
        assert!(
            !fds.contains(&leaked.to_string()),
            "the shell inherited fd {leaked}: {fds:?}"
        );
    }
}
