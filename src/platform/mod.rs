//! Process, terminal, filesystem and IPC plumbing on Linux: the libc,
//! `/proc` and helper-program calls the rest of the tree goes through.
//! shepr runs on Linux only, so this is one flat module with no per-OS
//! layer; the submodules hold self-contained pieces with their own tests.

use std::{
    collections::{HashSet, VecDeque},
    io::{Read, Write},
    os::fd::{AsRawFd, RawFd},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

mod remote_bridge;
#[cfg(test)]
mod remote_bridge_tests;
mod shutdown;
pub(crate) mod ssh_agent;
#[cfg(test)]
mod tests;

pub(crate) use shutdown::HostShutdownMonitor;

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundProcess {
    pub pid: u32,
    pub name: String,
    pub argv0: Option<String>,
    pub argv: Option<Vec<String>>,
    pub cmdline: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundJob {
    pub process_group_id: u32,
    pub processes: Vec<ForegroundProcess>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Hangup,
    Terminate,
    Kill,
}

/// Why a pane runtime ended, before application persistence policy is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExitReason {
    Exited,
    Interrupted,
    WaitFailed,
}

impl ChildExitReason {
    pub(crate) fn requires_session_checkpoint(self) -> bool {
        matches!(self, Self::Interrupted)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardCommand {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LimitedRead {
    Empty,
    Complete(Vec<u8>),
    Oversized,
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteSshConfigPaths {
    pub(crate) user_config: Option<PathBuf>,
    pub(crate) system_config: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Child exit, raw fds
// ---------------------------------------------------------------------------

pub(crate) fn classify_child_exit(status: &std::process::ExitStatus) -> ChildExitReason {
    use std::os::unix::process::ExitStatusExt;

    if status.signal().is_some() {
        ChildExitReason::Interrupted
    } else {
        ChildExitReason::Exited
    }
}

pub(crate) fn read_fd(fd: RawFd, data: &mut [u8]) -> std::io::Result<usize> {
    // SAFETY: read(2) writes at most `data.len()` bytes into `data`, a live
    // exclusive borrow; a bad fd fails with EBADF.
    let result = unsafe { libc::read(fd, data.as_mut_ptr().cast(), data.len()) };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(result.cast_unsigned())
    }
}

/// Wait up to `timeout_ms` (-1: forever) for `events` on `fd`. True when
/// poll reported anything, including POLLHUP/POLLERR, which the next read or
/// write then turns into EOF or an error.
fn poll_fd(fd: RawFd, events: libc::c_short, timeout_ms: i32) -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: one pollfd that lives on this stack frame for the call.
    let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(result > 0)
    }
}

pub(crate) fn poll_fd_readable(fd: RawFd, timeout_ms: i32) -> std::io::Result<bool> {
    poll_fd(fd, libc::POLLIN, timeout_ms)
}

/// Milliseconds left until `deadline` as a poll timeout, at least 1 so a
/// wait that is nearly due still sleeps instead of spinning. `None` once the
/// deadline has passed.
fn poll_timeout_until(deadline: Instant) -> Option<i32> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return None;
    }
    Some(i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX))
}

pub(crate) fn read_limited_reader(
    mut reader: impl Read,
    max_bytes: usize,
) -> std::io::Result<LimitedRead> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];

    while bytes.len() < max_bytes {
        let remaining = max_bytes - bytes.len();
        let read_len = remaining.min(buffer.len());
        let bytes_read = match reader.read(&mut buffer[..read_len]) {
            Ok(bytes_read) => bytes_read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if bytes_read == 0 {
            return if bytes.is_empty() {
                Ok(LimitedRead::Empty)
            } else {
                Ok(LimitedRead::Complete(bytes))
            };
        }
        bytes.extend_from_slice(&buffer[..bytes_read]);
    }

    let mut sentinel = [0_u8; 1];
    loop {
        return match reader.read(&mut sentinel) {
            Ok(0) if bytes.is_empty() => Ok(LimitedRead::Empty),
            Ok(0) => Ok(LimitedRead::Complete(bytes)),
            Ok(_) => Ok(LimitedRead::Oversized),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => Err(err),
        };
    }
}

// ---------------------------------------------------------------------------
// Host terminal and process setup
// ---------------------------------------------------------------------------

pub(crate) fn terminal_grid_size() -> std::io::Result<(u16, u16)> {
    let size = crossterm::terminal::window_size()?;
    let (cols, rows) = (size.columns, size.rows);
    if cols == 0 || rows == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "terminal reported a zero-sized grid",
        ));
    }
    Ok((cols, rows))
}

/// Raised by the SIGWINCH handler, consumed by the host resize watcher.
static TERMINAL_RESIZE_SIGNALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

extern "C" fn record_terminal_resize_signal(_signal: libc::c_int) {
    TERMINAL_RESIZE_SIGNALLED.store(true, std::sync::atomic::Ordering::Release);
}

/// Records SIGWINCH events that size polling can miss.
pub(crate) fn watch_terminal_resize_signal() {
    // SAFETY: sigaction is a plain C struct; all-zero is a valid value (no
    // flags, empty mask, SIG_DFL), and the fields that matter are set below.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction =
        record_terminal_resize_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // Keep blocking stdin and socket reads from failing with EINTR.
    action.sa_flags = libc::SA_RESTART;
    // SAFETY: `action` is a live local that sigaction only reads, the old
    // action pointer is null, and the handler is async-signal-safe: it does
    // one atomic store and nothing else.
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut());
    }
}

/// Returns whether a terminal size change was signalled since the last call.
pub(crate) fn take_terminal_resize_signal() -> bool {
    TERMINAL_RESIZE_SIGNALLED.swap(false, std::sync::atomic::Ordering::AcqRel)
}

fn set_sigpipe_disposition(handler: libc::sighandler_t) {
    // SAFETY: sigaction is a plain C struct; all-zero is a valid value.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler;
    // SAFETY: `action` is a live local that sigaction only reads; the old
    // action pointer is null. `handler` is SIG_DFL or SIG_IGN, not a function.
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        // Rust starts with SIGPIPE ignored. If this best-effort transition
        // fails, stdout retains the existing Rust behavior.
        libc::sigaction(libc::SIGPIPE, &action, std::ptr::null_mut());
    }
}

pub(crate) fn begin_cli_output() {
    set_sigpipe_disposition(libc::SIG_DFL);
}

pub fn detach_server_daemon_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: the pre-exec closure runs in the forked child and only calls
    // setsid(2), which is async-signal-safe and touches no memory.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Whether this server runs detached from any terminal, so closing the
/// terminal or SSH session that started it cannot hang it up. Remote attach
/// restarts a server that is not detached as a daemon.
///
/// Leading a session is not enough on its own: a terminal emulator or sshd
/// also makes the program it starts a session leader, but gives that session
/// the terminal as its controlling tty, and closing it sends SIGHUP. The
/// daemon spawn path calls setsid and never opens a terminal, so it leads its
/// session with no controlling tty; that pair is the test.
pub fn current_process_is_detached_server_daemon() -> bool {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return false;
    };
    session_and_tty_from_stat(&stat)
        .is_some_and(|(session, tty_nr)| is_detached_session(std::process::id(), session, tty_nr))
}

fn is_detached_session(pid: u32, session: i32, tty_nr: i32) -> bool {
    i64::from(session) == i64::from(pid) && tty_nr == 0
}

/// The path to run to start this program again. Use this, never raw
/// `current_exe()`, for anything that re-executes shepr or hands its path to
/// another process: once an install replaces the binary, Linux reports the
/// running one as "/…/shepr (deleted)", a path nothing can execute.
pub(crate) fn launch_executable() -> std::io::Result<PathBuf> {
    Ok(resolve_launch_executable(
        std::env::current_exe()?,
        Path::is_file,
    ))
}

fn resolve_launch_executable(executable: PathBuf, is_file: impl Fn(&Path) -> bool) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;

    if !is_file(&executable) {
        // Linux marks the old inode as deleted after an update replaces the binary.
        if let Some(path) = executable
            .as_os_str()
            .as_bytes()
            .strip_suffix(b" (deleted)")
        {
            let replacement = PathBuf::from(std::ffi::OsStr::from_bytes(path));
            if is_file(&replacement) {
                return replacement;
            }
        }
    }
    executable
}

const WSL_MARKER_ENV_VARS: &[&str] = &["WSL_DISTRO_NAME", "WSL_INTEROP"];

pub(crate) fn should_draw_host_cursor_by_default() -> bool {
    running_inside_wsl()
}

pub(crate) fn should_query_host_terminal_palette() -> bool {
    !running_inside_wsl()
}

fn running_inside_wsl() -> bool {
    static RUNNING_INSIDE_WSL: OnceLock<bool> = OnceLock::new();
    *RUNNING_INSIDE_WSL.get_or_init(detect_running_inside_wsl)
}

fn detect_running_inside_wsl() -> bool {
    proc_file_indicates_wsl("/proc/sys/kernel/osrelease")
        || proc_file_indicates_wsl("/proc/version")
        || WSL_MARKER_ENV_VARS
            .iter()
            .any(|key| std::env::var_os(key).is_some())
        || Path::new("/run/WSL").exists()
}

fn proc_file_indicates_wsl(path: &str) -> bool {
    std::fs::read_to_string(path)
        .map(|text| text_indicates_wsl(&text))
        .unwrap_or(false)
}

fn text_indicates_wsl(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("microsoft") || text.contains("wsl")
}

/// The machine's node name, as shown by tmux's `#h`.
pub(crate) fn hostname() -> Option<String> {
    let mut buffer = [0_u8; 256];
    // SAFETY: gethostname(2) writes at most `buffer.len()` bytes into a live
    // stack buffer.
    let result =
        unsafe { libc::gethostname(buffer.as_mut_ptr().cast::<libc::c_char>(), buffer.len()) };
    if result != 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]).into_owned();
    (!name.is_empty()).then_some(name)
}

pub(crate) fn local_datetime() -> Option<time::PrimitiveDateTime> {
    let mut timestamp: libc::time_t = 0;
    // SAFETY: time(2) writes one time_t into a live local.
    if unsafe { libc::time(&mut timestamp) } == -1 {
        return None;
    }
    // SAFETY: tm is a plain C struct of integers and a pointer; all-zero
    // (a null zone name) is a valid value, and localtime_r overwrites it.
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are live locals; localtime_r is the reentrant
    // form and keeps no reference to either.
    if unsafe { libc::localtime_r(&timestamp, &mut local) }.is_null() {
        return None;
    }
    datetime_from_tm(&local)
}

fn datetime_from_tm(value: &libc::tm) -> Option<time::PrimitiveDateTime> {
    let month = time::Month::try_from(u8::try_from(value.tm_mon + 1).ok()?).ok()?;
    let date = time::Date::from_calendar_date(
        value.tm_year + 1900,
        month,
        u8::try_from(value.tm_mday).ok()?,
    )
    .ok()?;
    let time = time::Time::from_hms(
        u8::try_from(value.tm_hour).ok()?,
        u8::try_from(value.tm_min).ok()?,
        u8::try_from(value.tm_sec).ok()?,
    )
    .ok()?;
    Some(time::PrimitiveDateTime::new(date, time))
}

fn effective_uid() -> libc::uid_t {
    // SAFETY: geteuid(2) takes no arguments, cannot fail and touches no memory.
    unsafe { libc::geteuid() }
}

// ---------------------------------------------------------------------------
// Local client streams
// ---------------------------------------------------------------------------

fn shutdown_client_stream(stream: &crate::ipc::LocalStream) -> std::io::Result<()> {
    let crate::ipc::LocalStream::UdSocket(stream) = stream;
    stream.inner().shutdown(std::net::Shutdown::Both)
}

pub(crate) struct ClientStreamReader<'a>(pub(crate) &'a mut crate::ipc::LocalStream);

impl Read for ClientStreamReader<'_> {
    fn read(&mut self, data: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.0.read(data) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let crate::ipc::LocalStream::UdSocket(stream) = &*self.0;
                    // Sleep until input or shutdown, without polling quiet observers.
                    if let Err(error) = poll_fd_readable(stream.inner().as_raw_fd(), -1)
                        && error.kind() != std::io::ErrorKind::Interrupted
                    {
                        return Err(error);
                    }
                }
                result => return result,
            }
        }
    }
}

pub(crate) fn write_client_stream(
    stream: &crate::ipc::LocalStream,
    mut data: &[u8],
) -> std::io::Result<()> {
    use std::io;

    let crate::ipc::LocalStream::UdSocket(socket) = stream;
    let mut socket = socket.inner();
    let Some(timeout) = socket.write_timeout()? else {
        return socket.write_all(data);
    };
    let timed_out = || {
        // Dropping the writer clone alone would leave the reader blocked.
        let _ = shutdown_client_stream(stream);
        io::Error::new(
            io::ErrorKind::TimedOut,
            "terminal observer stopped receiving output",
        )
    };
    let mut progress = Instant::now();
    while !data.is_empty() {
        match socket.write(data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => {
                data = &data[written..];
                progress = Instant::now();
                continue;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
        let wait_ms = poll_timeout_until(progress + timeout).ok_or_else(timed_out)?;
        match poll_fd(socket.as_raw_fd(), libc::POLLOUT, wait_ms) {
            Ok(false) => return Err(timed_out()),
            Ok(true) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(crate) fn wait_client_stream_readable(stream: &crate::ipc::LocalStream) -> std::io::Result<()> {
    use std::os::fd::AsFd as _;
    let crate::ipc::LocalStream::UdSocket(stream) = stream;
    // Bound cancellation latency without polling idle connections hundreds of times per second.
    match poll_fd_readable(stream.as_fd().as_raw_fd(), 100) {
        Err(error) if error.kind() != std::io::ErrorKind::Interrupted => Err(error),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Remote bridge stdio
// ---------------------------------------------------------------------------

pub(crate) fn forward_remote_bridge_stdio(
    stream: crate::ipc::LocalStream,
    idle_timeout: bool,
) -> std::io::Result<()> {
    forward_remote_bridge_stdio_with_timeout(
        stream,
        idle_timeout.then_some(remote_bridge::IDLE_TIMEOUT),
    )
}

fn forward_remote_bridge_stdio_with_timeout(
    stream: crate::ipc::LocalStream,
    idle_timeout: Option<Duration>,
) -> std::io::Result<()> {
    use interprocess::TryClone as _;
    use remote_bridge::{Activity, TrackedIo};

    let activity = idle_timeout.map(Activity::start).transpose()?;
    let mut stdout = TrackedIo::new(std::io::stdout().lock(), activity.clone());
    let mut socket_to_stdout = TrackedIo::new(stream.try_clone()?, activity.clone());
    let mut stdin_to_socket = stream;
    let _upload = std::thread::spawn(move || {
        let mut stdin = TrackedIo::new(std::io::stdin(), activity.clone());
        let _ = copy_flush(
            &mut stdin,
            &mut TrackedIo::new(&mut stdin_to_socket, activity),
        );
        let crate::ipc::LocalStream::UdSocket(stream) = stdin_to_socket;
        let _ = stream.inner().shutdown(std::net::Shutdown::Write);
    });
    copy_flush(&mut socket_to_stdout, &mut stdout)
}

fn copy_flush<R: Read, W: Write>(reader: &mut R, writer: &mut W) -> std::io::Result<()> {
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        writer.write_all(&buffer[..read])?;
        writer.flush()?;
    }
}

pub(crate) struct RemoteBridgeWake {
    reader: std::os::unix::net::UnixStream,
    writer: std::os::unix::net::UnixStream,
}

impl RemoteBridgeWake {
    pub(crate) fn new() -> std::io::Result<Self> {
        let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
        Ok(Self { reader, writer })
    }

    pub(crate) fn cancel(&self) -> std::io::Result<()> {
        // EOF stays readable, including when cancellation precedes the wait.
        self.writer.shutdown(std::net::Shutdown::Write)
    }

    pub(crate) fn wait(&self, stream: &crate::ipc::LocalStream) -> std::io::Result<()> {
        use std::os::fd::AsFd as _;
        let crate::ipc::LocalStream::UdSocket(stream) = stream;
        let mut descriptors = [
            libc::pollfd {
                fd: stream.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: both descriptors remain borrowed and the array has two entries.
            if unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) } >= 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SSH paths and private files
// ---------------------------------------------------------------------------

/// The longest socket path Linux accepts: `sun_path` is 108 bytes, one of
/// them the terminating NUL.
const UNIX_SOCKET_PATH_MAX: usize = 107;

pub(crate) fn fits_unix_socket_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().len() <= UNIX_SOCKET_PATH_MAX
}

pub(crate) fn remote_ssh_config_paths() -> RemoteSshConfigPaths {
    RemoteSshConfigPaths {
        user_config: std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".ssh").join("config")),
        system_config: Some(PathBuf::from("/etc/ssh/ssh_config")),
    }
}

pub(crate) fn create_remote_ssh_config_dir(control_socket_name: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let mut bases = vec![std::env::temp_dir()];
    let short_tmp = PathBuf::from("/tmp");
    if bases.first() != Some(&short_tmp) {
        bases.push(short_tmp);
    }

    let mut last_error = None;
    let mut path_fits = false;
    for base in bases {
        for attempt in 0..100 {
            let dir = base.join(format!("shepr-ssh-{}-{attempt}", std::process::id()));
            if !fits_unix_socket_path(&dir.join(control_socket_name)) {
                continue;
            }
            path_fits = true;
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(dir),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    last_error = Some(err);
                    break;
                }
            }
        }
    }

    if let Some(err) = last_error {
        return Err(err);
    }
    let message = if path_fits {
        "failed to create private shepr ssh config directory"
    } else {
        "SSH control socket path exceeds the Unix socket length limit"
    };
    Err(std::io::Error::new(
        if path_fits {
            std::io::ErrorKind::AlreadyExists
        } else {
            std::io::ErrorKind::InvalidInput
        },
        message,
    ))
}

/// Create a new file only the current user can read or write. Fails if the
/// path already exists.
pub(crate) fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// A path in the shared temp directory for an SSH bridge socket. Callers
/// compute it once and carry it; it is not derivable again.
///
/// The names callers pass are pid-derived and so predictable, and the temp
/// directory is shared: another user could leave a socket at the exact path
/// first, and this user cannot remove it, so the bind would fail. A random
/// token goes into every name ("<stem>.<token>.sock") so such a squat has to
/// guess it. The bind is owner-only and the accept checks its peer either
/// way; this only keeps a connect attempt from being blocked.
pub(crate) fn remote_bridge_endpoint_path(readable_name: &str, short_name: &str) -> PathBuf {
    let token = unpredictable_token();
    let readable_name = with_name_token(readable_name, token);
    let short_name = with_name_token(short_name, token);
    let tmp = std::env::temp_dir();
    let readable = tmp.join(&readable_name);
    if fits_unix_socket_path(&readable) {
        return readable;
    }
    let short = tmp.join(&short_name);
    if fits_unix_socket_path(&short) {
        return short;
    }
    PathBuf::from("/tmp").join(short_name)
}

/// `name` with `.{token:016x}` inserted before its extension, or appended
/// when it has none.
fn with_name_token(name: &str, token: u64) -> String {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem}.{token:016x}.{extension}"),
        _ => format!("{name}.{token:016x}"),
    }
}

/// 64 bits another local user cannot predict: getrandom(2), or std's
/// OS-seeded hasher keys if that fails.
fn unpredictable_token() -> u64 {
    use std::hash::{BuildHasher, Hasher};

    let mut bytes = [0_u8; 8];
    // SAFETY: getrandom(2) writes at most `bytes.len()` bytes into a live
    // stack buffer and keeps no reference to it.
    let filled = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if usize::try_from(filled).is_ok_and(|filled| filled == bytes.len()) {
        return u64::from_ne_bytes(bytes);
    }
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    hasher.finish()
}

/// Shared OpenSSH sockets outlive individual helpers. Never adopt a directory
/// belonging to another uid, a symlink, or a directory accessible by others.
pub(crate) fn shared_ssh_control_path(namespace: &Path, target: &str) -> std::io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt},
    };

    // Validate the resolved system temp directory, but retain the short /tmp
    // spelling for sockets so OpenSSH keeps room for its staging suffix.
    let base = Path::new("/tmp");
    let resolved_base = std::fs::canonicalize(base)?;
    let metadata = std::fs::symlink_metadata(&resolved_base)?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o1000 == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe SSH control directory parent",
        ));
    }
    let dir = base.join(format!("hssh-{}", effective_uid()));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    validate_shared_ssh_dir(&dir)?;
    let namespace = if namespace.is_absolute() {
        namespace.to_owned()
    } else {
        std::env::current_dir()?.join(namespace)
    };
    let mut hash = Sha256::new();
    hash.update(namespace.as_os_str().as_bytes());
    hash.update([0]);
    hash.update(target.as_bytes());
    // %C additionally scopes the socket to OpenSSH's resolved destination,
    // port and jump host, rather than merely the spelling of an alias.
    // Keep 96 bits of namespace/target hash plus OpenSSH's 160-bit %C.
    let digest = hash.finalize();
    let mut hash = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hash, "{byte:02x}");
    }
    let path = dir.join(format!("{}-%C", &hash[..24]));
    // OpenSSH first binds ControlPath + '.' + 16 random characters, then
    // renames it. Reserve those 17 bytes, not just the final socket's length.
    let expanded = path.to_string_lossy().replace("%C", &"0".repeat(40));
    let staging = PathBuf::from(format!("{expanded}.{}", "0".repeat(16)));
    if !fits_unix_socket_path(&staging) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "SSH control socket staging path exceeds the Unix socket length limit",
        ));
    }
    Ok(path)
}

fn validate_shared_ssh_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || metadata.uid() != effective_uid() || metadata.mode() & 0o7777 != 0o700
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "SSH control directory must be owned by the current user, mode 0700, and not a symlink",
        ));
    }
    Ok(())
}

/// Fsyncs `directory` itself, making a rename or unlink inside it durable.
/// Callers pass the directory that holds the entry they just changed, not the
/// entry.
pub(crate) fn sync_directory(directory: &Path) -> std::io::Result<()> {
    std::fs::File::open(directory)?.sync_all()
}

// ---------------------------------------------------------------------------
// Config file replacement
// ---------------------------------------------------------------------------

pub(crate) fn config_file_link_count(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.nlink())
}

pub(crate) fn create_config_temporary(
    path: &Path,
    private: bool,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if private { 0o600 } else { 0o666 })
        .open(path)
}

pub(crate) fn write_config_temporary(
    source: Option<&Path>,
    temporary: &Path,
    contents: &[u8],
) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(temporary)?;
    if let Some(source) = source {
        let input = std::fs::File::open(source)?;
        let metadata = input.metadata()?;
        let current = output.metadata()?;
        if (metadata.uid(), metadata.gid()) != (current.uid(), current.gid()) {
            // Keep ownership before restoring mode/ACLs; chown can clear mode bits.
            // SAFETY: fchown(2) on an fd `output` keeps open; integers only.
            if unsafe { libc::fchown(output.as_raw_fd(), metadata.uid(), metadata.gid()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        // Replace inherited ACLs before enabling the original mode. Prepare all
        // access controls while the temporary is empty, before writing secrets.
        copy_config_xattrs(input.as_raw_fd(), output.as_raw_fd())?;
        output.set_permissions(metadata.permissions())?;
    }
    output.write_all(contents)?;
    output.sync_all()
}

// Access ACLs and security labels live in xattrs on Linux. Mode bits alone can
// silently broaden access, especially with a default ACL on the parent directory.
fn copy_config_xattrs(source: RawFd, destination: RawFd) -> std::io::Result<()> {
    use std::ffi::CStr;
    fn names(fd: RawFd) -> std::io::Result<Vec<u8>> {
        // SAFETY: a null buffer of size 0 asks flistxattr(2) for the size
        // only; nothing is written.
        let size = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0) };
        if size < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOTSUP) {
                return Ok(Vec::new());
            }
            return Err(error);
        }
        let size = usize::try_from(size).unwrap_or(0);
        let mut buffer = vec![0; size];
        // SAFETY: writes at most `buffer.len()` bytes into the live buffer.
        let read = unsafe { libc::flistxattr(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 {
            return Err(std::io::Error::last_os_error());
        }
        buffer.truncate(usize::try_from(read).unwrap_or(0));
        Ok(buffer)
    }
    fn value(fd: RawFd, name: &CStr) -> std::io::Result<Vec<u8>> {
        // SAFETY: `name` is NUL-terminated; a null buffer of size 0 asks for
        // the size only.
        let size = unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let size = usize::try_from(size).unwrap_or(0);
        let mut buffer = vec![0; size];
        // SAFETY: `name` is NUL-terminated; writes at most `buffer.len()`
        // bytes into the live buffer.
        let read =
            unsafe { libc::fgetxattr(fd, name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 {
            return Err(std::io::Error::last_os_error());
        }
        buffer.truncate(usize::try_from(read).unwrap_or(0));
        Ok(buffer)
    }
    let source_names = names(source)?;
    for bytes in names(destination)?.split_inclusive(|byte| *byte == 0) {
        if !source_names
            .split_inclusive(|byte| *byte == 0)
            .any(|name| name == bytes)
        {
            let name = CStr::from_bytes_with_nul(bytes).map_err(std::io::Error::other)?;
            // SAFETY: `name` is NUL-terminated and outlives the call.
            if unsafe { libc::fremovexattr(destination, name.as_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    for bytes in source_names.split_inclusive(|byte| *byte == 0) {
        let name = CStr::from_bytes_with_nul(bytes).map_err(std::io::Error::other)?;
        let original = value(source, name)?;
        // Avoid requiring relabel privileges when the inherited label already matches.
        if value(destination, name).is_ok_and(|current| current == original) {
            continue;
        }
        // SAFETY: `name` is NUL-terminated; fsetxattr reads `original.len()`
        // bytes from the live buffer.
        if unsafe {
            libc::fsetxattr(
                destination,
                name.as_ptr(),
                original.as_ptr().cast(),
                original.len(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Status commands
// ---------------------------------------------------------------------------

pub(crate) fn configure_status_command(process: &mut Command) {
    use std::os::unix::process::CommandExt;

    process.process_group(0);
}

pub(crate) struct StatusCommandGuard {
    process_group_id: Option<i32>,
    /// A handle on the group leader, opened while the child was certainly
    /// unreaped. `None` only if it could not be opened at all.
    leader: Option<ProcessHandle>,
}

impl StatusCommandGuard {
    pub(crate) fn new(child: &tokio::process::Child) -> std::io::Result<Self> {
        // `id()` is `None` once tokio has reaped the child, and reaping needs
        // `&mut Child`, so the pid cannot be reused before the handle is open.
        let process_id = child
            .id()
            .ok_or_else(|| std::io::Error::other("status command has no process id"))?;
        let process_group_id = i32::try_from(process_id)
            .map_err(|_| std::io::Error::other("status command process id exceeds i32"))?;
        Ok(Self {
            process_group_id: Some(process_group_id),
            leader: ProcessHandle::open(process_id),
        })
    }

    pub(crate) fn terminate(&mut self) {
        let Some(process_group_id) = self.process_group_id.take() else {
            return;
        };
        let leader = self.leader.take();
        // The command was spawned as this process group's leader. Killing the
        // group also cleans up background descendants on completion or
        // cancellation, but only while the id still names that group: tokio
        // may have reaped the leader already, and a reused number would send
        // SIGKILL to an unrelated group. The remaining gap (the number is
        // reused, the new owner leads a group and exits, all between the reap
        // and this call) needs a full pid wraparound in that window.
        let ours = status_group_is_ours(leader.as_ref().map(ProcessHandle::is_unreaped), || {
            Path::new(&format!("/proc/{process_group_id}")).exists()
        });
        if !ours {
            return;
        }
        // SAFETY: kill(2) touches no memory of this process.
        unsafe {
            libc::kill(-process_group_id, libc::SIGKILL);
        }
    }
}

/// Whether process group `process_group_id` can still only be the one the
/// status command led. The kernel reuses a number only once nothing holds it
/// as a pid, process-group id or session id. An unreaped leader holds it.
/// After the leader is reaped, any task that holds that pid again is proof
/// the number was reused, and the original group had no members left when
/// that happened. Without a leader handle the second test is all there is.
fn status_group_is_ours(
    leader_unreaped: Option<bool>,
    pid_held_by_a_task: impl FnOnce() -> bool,
) -> bool {
    leader_unreaped == Some(true) || !pid_held_by_a_task()
}

impl Drop for StatusCommandGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

// ---------------------------------------------------------------------------
// Shell commands
// ---------------------------------------------------------------------------

pub(crate) fn interactive_shell_command(argv: &[String]) -> Option<String> {
    let mut parts = argv.iter();
    let mut command = shell_quote(parts.next()?);
    for part in parts {
        command.push(' ');
        command.push_str(&shell_quote(part));
    }
    Some(command)
}

pub(crate) fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn is_pane_shell_process_name(name: &str) -> bool {
    let normalized = name
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .trim_start_matches('-')
        .to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "ksh"
            | "mksh"
            | "csh"
            | "tcsh"
            | "elvish"
            | "xonsh"
            | "nu"
    )
}

// ---------------------------------------------------------------------------
// Foreground job detection
// ---------------------------------------------------------------------------

const PROCESS_DETECTION_ENV_VAR: &str = "SHEPR_PROCESS_DETECTION";
const CHILD_GROUPS_SCAN_LIMIT: usize = 64;
/// Upper bound on the number of processes visited while resolving a pane's
/// foreground process-group tree. Foreground-job detection reads /proc/<pid>/stat
/// and task/children files for every visited process on a repeated (per-tick/5s)
/// cadence, so an unbounded walk lets accumulated descendants or unreaped zombies
/// under the pane shell grow the server's read-syscall rate and CPU without limit
/// at a constant pane count (see AGENTS.md multiplicative performance paths). The
/// foreground-group leader's subtree and the pane shell's descendants advance
/// round-robin under a shared candidate ceiling, with independent per-root work
/// budgets, so a pathologically large accumulation on either side cannot starve the
/// other. Discovery is best effort once a budget is exhausted.
const FOREGROUND_TREE_SCAN_LIMIT: usize = 512;
/// Number of `/proc/<pid>/task` entries a root's subtree may consume, bounding how
/// far one process's thread count can multiply the walk's work.
const FOREGROUND_TASK_ENTRY_LIMIT: usize = 2_048;
/// Number of `/proc/<pid>/task/<tid>/children` bytes a root's subtree may read,
/// stopping a parent that accumulates unreaped children from growing read work
/// without limit.
const FOREGROUND_CHILD_BYTE_LIMIT: usize = 128 * 1024;
/// Aggregate number of child pids a root's subtree may parse and enqueue, bounding
/// the walk's pending queues and allocations.
const FOREGROUND_CHILD_PID_LIMIT: usize = 2_048;

/// Per-root work budget for foreground process discovery. The foreground-group
/// leader and the pane shell each get their own budget, so one side's expansion
/// cannot exhaust the other's allowance. Every task entry, child byte, and parsed
/// child pid is charged against the owning root's budget, keeping total `/proc` work
/// bounded independently of the process-tree size and of uptime. Discovery is best
/// effort once a budget is exhausted.
#[derive(Debug)]
struct ForegroundScanBudget {
    task_entries: usize,
    child_bytes: usize,
    child_pids: usize,
}

impl ForegroundScanBudget {
    fn for_probe() -> Self {
        Self {
            task_entries: FOREGROUND_TASK_ENTRY_LIMIT,
            child_bytes: FOREGROUND_CHILD_BYTE_LIMIT,
            child_pids: FOREGROUND_CHILD_PID_LIMIT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessDetectionMode {
    Native,
    ChildGroups,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcGroupMember {
    pid: u32,
    comm: String,
    state: char,
}

fn parse_process_detection_mode(value: Option<&str>) -> Result<ProcessDetectionMode, &str> {
    match value {
        None | Some("") | Some("native") => Ok(ProcessDetectionMode::Native),
        Some("child-groups") => Ok(ProcessDetectionMode::ChildGroups),
        Some(value) => Err(value),
    }
}

fn process_detection_mode() -> ProcessDetectionMode {
    static MODE: OnceLock<ProcessDetectionMode> = OnceLock::new();
    *MODE.get_or_init(|| {
        let value = std::env::var(PROCESS_DETECTION_ENV_VAR).ok();
        parse_process_detection_mode(value.as_deref()).unwrap_or_else(|value| {
            tracing::warn!(
                variable = PROCESS_DETECTION_ENV_VAR,
                %value,
                "unknown process detection mode; using native detection"
            );
            ProcessDetectionMode::Native
        })
    })
}

/// Collect the foreground terminal job for a given child PID.
pub(crate) fn available_pane_shell(child_pid: u32) -> Option<String> {
    available_pane_shell_from_job(child_pid, foreground_job(child_pid)?)
}

pub(crate) fn available_pane_shell_from_job(child_pid: u32, job: ForegroundJob) -> Option<String> {
    if job.process_group_id != child_pid
        || job.processes.iter().any(|process| process.pid != child_pid)
    {
        return None;
    }
    job.processes
        .into_iter()
        .find(|process| process.pid == child_pid)
        .map(|process| process.name)
        .filter(|name| is_pane_shell_process_name(name))
}

pub fn foreground_job(child_pid: u32) -> Option<ForegroundJob> {
    let process_group_id = foreground_process_group_id(child_pid).or_else(|| {
        (process_detection_mode() == ProcessDetectionMode::ChildGroups)
            .then(|| child_groups_foreground_process_group(child_pid))
            .flatten()
    })?;
    foreground_job_for_group(child_pid, process_group_id)
}

fn foreground_job_for_group(child_pid: u32, process_group_id: u32) -> Option<ForegroundJob> {
    let members = foreground_process_group_members(child_pid, process_group_id)?;
    foreground_job_from_members(
        process_group_id,
        members,
        running_inside_wsl(),
        process_argv,
    )
}

fn foreground_job_from_members(
    process_group_id: u32,
    members: Vec<ProcGroupMember>,
    running_inside_wsl: bool,
    mut read_argv: impl FnMut(u32) -> Option<Vec<String>>,
) -> Option<ForegroundJob> {
    let processes = members
        .into_iter()
        .map(|member| {
            // Reading procfs cmdline enters access_remote_vm. On WSL, that read can
            // block indefinitely while a multithreaded process is exiting. A state
            // check alone has a race, so WSL uses the cheap comm-based identity when
            // it already identifies a supported agent without inspecting cmdline.
            let argv =
                process_allows_remote_memory_read(member.state, &member.comm, running_inside_wsl)
                    .then(|| read_argv(member.pid))
                    .flatten();
            ForegroundProcess {
                pid: member.pid,
                name: member.comm,
                argv0: None,
                cmdline: argv.as_ref().map(|parts| parts.join(" ")),
                argv,
            }
        })
        .collect::<Vec<_>>();

    if processes.is_empty() {
        return None;
    }

    Some(ForegroundJob {
        process_group_id,
        processes,
    })
}

/// Best-effort foreground group for environments that do not expose terminal
/// foreground groups. This mode is explicit because background jobs cannot be
/// distinguished from foreground jobs without the native terminal signal.
fn child_groups_foreground_process_group(child_pid: u32) -> Option<u32> {
    let shell_group_id = u32::try_from(
        process_pgrp_comm_and_state(child_pid)
            .map(|(pgrp, _, _)| pgrp)
            .filter(|pgrp| *pgrp > 0)?,
    )
    .ok()?;

    child_groups_foreground_process_group_with(
        child_pid,
        shell_group_id,
        process_task_ids,
        process_task_children,
        |pid| process_pgrp_comm_and_state(pid).map(|(pgrp, _, _)| pgrp),
    )
}

fn child_groups_foreground_process_group_with(
    child_pid: u32,
    shell_group_id: u32,
    mut task_ids: impl FnMut(u32, &mut ForegroundScanBudget) -> Vec<u32>,
    mut task_children: impl FnMut(u32, u32, &mut ForegroundScanBudget) -> Vec<u32>,
    mut process_group_id: impl FnMut(u32) -> Option<i32>,
) -> Option<u32> {
    let mut budget = ForegroundScanBudget::for_probe();
    let mut newest = None;
    let mut scanned = 0usize;
    for tid in task_ids(child_pid, &mut budget) {
        for child in task_children(child_pid, tid, &mut budget) {
            if scanned >= CHILD_GROUPS_SCAN_LIMIT {
                return None;
            }
            scanned += 1;

            let Some(pgrp) = process_group_id(child) else {
                continue;
            };
            if pgrp <= 0 {
                continue;
            }
            let Ok(pgrp) = u32::try_from(pgrp) else {
                continue;
            };
            if pgrp == shell_group_id {
                continue;
            }
            newest = Some(newest.map_or(pgrp, |current: u32| current.max(pgrp)));
        }
    }
    newest.or(Some(shell_group_id))
}

fn foreground_process_group_members(
    child_pid: u32,
    process_group_id: u32,
) -> Option<Vec<ProcGroupMember>> {
    foreground_process_group_members_with(
        child_pid,
        process_group_id,
        process_task_ids,
        process_task_children,
        live_process_group_member,
    )
}

fn foreground_process_group_members_with(
    child_pid: u32,
    process_group_id: u32,
    task_ids: impl FnMut(u32, &mut ForegroundScanBudget) -> Vec<u32>,
    task_children: impl FnMut(u32, u32, &mut ForegroundScanBudget) -> Vec<u32>,
    mut live_member: impl FnMut(u32, u32) -> Option<ProcGroupMember>,
) -> Option<Vec<ProcGroupMember>> {
    // The leader is passed first; `process_tree_pids` advances both roots round-robin
    // so a truncated scan cannot let the pane shell's unrelated descendants starve
    // the foreground group, or vice versa.
    let mut members = process_tree_pids([process_group_id, child_pid], task_ids, task_children)
        .into_iter()
        .filter_map(|pid| live_member(process_group_id, pid))
        .collect::<Vec<_>>();
    members.sort_unstable_by_key(|member| member.pid);
    (!members.is_empty()).then_some(members)
}

fn process_tree_pids(
    roots: impl IntoIterator<Item = u32>,
    mut task_ids: impl FnMut(u32, &mut ForegroundScanBudget) -> Vec<u32>,
    mut task_children: impl FnMut(u32, u32, &mut ForegroundScanBudget) -> Vec<u32>,
) -> Vec<u32> {
    // Keep one breadth-first frontier with its own work budget per root, so a large
    // expansion on one side cannot consume the other side's allowance. Frontier turns
    // advance round-robin, sharing the candidate ceiling between the foreground-group
    // leader's subtree and the pane shell's descendants.
    struct Frontier {
        pending: VecDeque<u32>,
        budget: ForegroundScanBudget,
    }

    let mut visited = HashSet::new();
    let mut pids = Vec::new();
    let mut frontiers: Vec<Frontier> = Vec::new();
    for root in roots {
        if root > 0 && visited.insert(root) {
            frontiers.push(Frontier {
                pending: VecDeque::from([root]),
                budget: ForegroundScanBudget::for_probe(),
            });
        }
    }

    loop {
        let mut progressed = false;
        for frontier in &mut frontiers {
            if pids.len() >= FOREGROUND_TREE_SCAN_LIMIT {
                return pids;
            }
            let Some(pid) = frontier.pending.pop_front() else {
                continue;
            };
            progressed = true;
            pids.push(pid);
            for tid in task_ids(pid, &mut frontier.budget) {
                for child_pid in task_children(pid, tid, &mut frontier.budget) {
                    if child_pid > 0 && visited.insert(child_pid) {
                        frontier.pending.push_back(child_pid);
                    }
                }
            }
        }
        if !progressed {
            return pids;
        }
    }
}

fn process_task_ids(pid: u32, budget: &mut ForegroundScanBudget) -> Vec<u32> {
    let mut ids = Vec::new();
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return ids;
    };
    for entry in entries.flatten() {
        if budget.task_entries == 0 {
            break;
        }
        budget.task_entries -= 1;
        if let Some(tid) = numeric_file_name(&entry) {
            ids.push(tid);
        }
    }
    ids
}

fn process_task_children(pid: u32, tid: u32, budget: &mut ForegroundScanBudget) -> Vec<u32> {
    if budget.child_bytes == 0 || budget.child_pids == 0 {
        return Vec::new();
    }
    let Ok(file) = std::fs::File::open(format!("/proc/{pid}/task/{tid}/children")) else {
        return Vec::new();
    };
    read_bounded_pid_list(file, budget)
}

/// Read a whitespace-separated pid list, charging the shared budget for every byte
/// read and every parsed pid. A token cut off by the byte budget is discarded so a
/// partial value is never parsed as a different pid; the final token is only kept
/// when the reader reaches end-of-file.
fn read_bounded_pid_list(mut reader: impl Read, budget: &mut ForegroundScanBudget) -> Vec<u32> {
    let mut pids = Vec::new();
    let mut token = Vec::new();
    let mut buffer = [0_u8; 4096];

    while budget.child_bytes > 0 && budget.child_pids > 0 {
        let read_len = budget.child_bytes.min(buffer.len());
        let bytes_read = match reader.read(&mut buffer[..read_len]) {
            Ok(0) => {
                push_pid_token(&mut pids, &mut token, budget);
                return pids;
            }
            Ok(bytes_read) => bytes_read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return pids,
        };
        budget.child_bytes -= bytes_read;
        for &byte in &buffer[..bytes_read] {
            if byte.is_ascii_digit() {
                token.push(byte);
                continue;
            }
            push_pid_token(&mut pids, &mut token, budget);
            if budget.child_pids == 0 {
                return pids;
            }
        }
    }

    // The byte budget ran out mid-stream: drop the trailing token in case the read
    // truncated it.
    pids
}

fn push_pid_token(pids: &mut Vec<u32>, token: &mut Vec<u8>, budget: &mut ForegroundScanBudget) {
    if token.is_empty() || budget.child_pids == 0 {
        token.clear();
        return;
    }
    if let Some(pid) = std::str::from_utf8(token)
        .ok()
        .and_then(|text| text.parse::<u32>().ok())
    {
        budget.child_pids -= 1;
        pids.push(pid);
    }
    token.clear();
}

fn numeric_file_name(entry: &std::fs::DirEntry) -> Option<u32> {
    let file_name = entry.file_name();
    let value = file_name.to_str()?;
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn live_process_group_member(process_group_id: u32, pid: u32) -> Option<ProcGroupMember> {
    let (pgrp, comm, state) = process_pgrp_comm_and_state(pid)?;
    (u32::try_from(pgrp) == Ok(process_group_id)).then_some(ProcGroupMember { pid, comm, state })
}

pub fn foreground_group_leader_job(process_group_id: u32) -> Option<ForegroundJob> {
    let (pgrp, name, state) = process_pgrp_comm_and_state(process_group_id)?;
    if u32::try_from(pgrp) != Ok(process_group_id) {
        return None;
    }

    let argv = process_allows_remote_memory_read(state, &name, running_inside_wsl())
        .then(|| process_argv(process_group_id))
        .flatten();
    Some(ForegroundJob {
        process_group_id,
        processes: vec![ForegroundProcess {
            pid: process_group_id,
            name,
            argv0: None,
            cmdline: argv.as_ref().map(|parts| parts.join(" ")),
            argv,
        }],
    })
}

pub fn foreground_process_group_id(child_pid: u32) -> Option<u32> {
    // /proc/<pid>/stat format: "pid (comm) state ppid pgrp session tty_nr tpgid ..."
    // The (comm) field can contain spaces and parens, so we find the last ')' first.
    let stat = std::fs::read_to_string(format!("/proc/{child_pid}/stat")).ok()?;
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After (comm): state(0) ppid(1) pgrp(2) session(3) tty_nr(4) tpgid(5)
    let tpgid: i32 = fields.get(5)?.parse().ok()?;
    (tpgid > 0).then(|| u32::try_from(tpgid).unwrap_or_default())
}

fn process_pgrp_comm_and_state(pid: u32) -> Option<(i32, String, char)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    process_pgrp_comm_and_state_from_stat(&stat)
}

fn process_pgrp_comm_and_state_from_stat(stat: &str) -> Option<(i32, String, char)> {
    let close = stat.rfind(')')?;
    let comm = stat.get(1 + stat.find('(')?..close)?.to_string();
    let rest = stat.get(close + 2..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let pgrp: i32 = fields.get(2)?.parse().ok()?;
    Some((pgrp, comm, state))
}

fn process_state_allows_remote_memory_read(state: char) -> bool {
    !matches!(state, 'D' | 'Z' | 'X' | 'x')
}

fn process_allows_remote_memory_read(state: char, comm: &str, running_inside_wsl: bool) -> bool {
    process_state_allows_remote_memory_read(state)
        && (!running_inside_wsl || crate::detect::identify_agent(comm).is_none())
}

fn process_argv(pid: u32) -> Option<Vec<String>> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let parts: Vec<String> = bytes
        .split(|&b| b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    (!parts.is_empty()).then_some(parts)
}

/// Get the current working directory of a process.
/// Uses /proc/<pid>/cwd symlink.
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    if pid == 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// Read a Shepr agent identity hint from a process environment.
pub fn process_agent_hint(pid: u32) -> Option<crate::detect::Agent> {
    if pid == 0 {
        return None;
    }
    let (_, comm, state) = process_pgrp_comm_and_state(pid)?;
    if !process_allows_remote_memory_read(state, &comm, running_inside_wsl()) {
        return None;
    }
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    parse_agent_env_hint(&environ)
}

pub(crate) fn parse_agent_env_hint(environ: &[u8]) -> Option<crate::detect::Agent> {
    for record in environ.split(|&byte| byte == 0) {
        let Some(value) = record.strip_prefix(b"SHEPR_AGENT=") else {
            continue;
        };
        return crate::detect::parse_agent_label(std::str::from_utf8(value).ok()?);
    }
    None
}

// ---------------------------------------------------------------------------
// Process handles and session teardown
// ---------------------------------------------------------------------------

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
pub(crate) struct ProcessHandle {
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
    pub(crate) fn open(pid: u32) -> Option<Self> {
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
    fn open_by_start_time(pid: u32) -> Option<Self> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, start_time) = state_and_start_time_from_stat(&stat)?;
        Some(Self {
            pid,
            identity: ProcessIdentity::StartTime(start_time),
        })
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    fn pidfd(&self) -> Option<RawFd> {
        match &self.identity {
            ProcessIdentity::Pidfd(fd) => Some(fd.as_raw_fd()),
            ProcessIdentity::StartTime(_) => None,
        }
    }

    /// For a start-time handle: this process's state letter while it still
    /// holds its pid (a zombie included), `None` once it has been reaped.
    fn state_by_start_time(&self, start_time: u64) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.pid)).ok()?;
        let (state, current) = state_and_start_time_from_stat(&stat)?;
        (current == start_time).then_some(state)
    }

    fn send(&self, signal: libc::c_int) -> bool {
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
    pub(crate) fn signal(&self, signal: Signal) -> bool {
        self.send(signal_number(signal))
    }

    /// Whether the process still holds its pid: running, or a zombie nobody
    /// has reaped yet. While this is true the pid cannot be reused, and
    /// neither can a process-group or session id equal to it.
    pub(crate) fn is_unreaped(&self) -> bool {
        match &self.identity {
            ProcessIdentity::Pidfd(_) => self.send(0),
            ProcessIdentity::StartTime(start_time) => {
                self.state_by_start_time(*start_time).is_some()
            }
        }
    }

    /// Whether the process has exited. A zombie counts as exited.
    pub(crate) fn has_exited(&self) -> bool {
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

/// The state letter and start time (clock ticks since boot) from a
/// `/proc/<pid>/stat` line. The command name is skipped by its last `)`.
fn state_and_start_time_from_stat(stat: &str) -> Option<(char, u64)> {
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    // starttime is field 22 of stat(5); `state` was field 3.
    let start_time = fields.nth(18)?.parse().ok()?;
    Some((state, start_time))
}

/// Wait until every handle's process has exited, or `timeout` passes.
/// Returns whether they all exited.
pub(crate) fn wait_for_process_exits(handles: &[&ProcessHandle], timeout: Duration) -> bool {
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
pub(crate) fn session_member_handles(
    session_id: u32,
    leader_reaped: impl Fn() -> bool,
) -> Vec<ProcessHandle> {
    session_member_handles_with(session_id, leader_reaped, ProcessHandle::open)
}

fn session_member_handles_with(
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

fn process_session_id(pid: u32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    session_and_tty_from_stat(&stat).map(|(session, _tty)| session)
}

/// The session id and controlling-terminal device (`tty_nr`, 0 for none) from
/// a `/proc/<pid>/stat` line. The command name is skipped by its last `)`, as
/// it may itself contain spaces and parentheses.
fn session_and_tty_from_stat(stat: &str) -> Option<(i32, i32)> {
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let mut fields = rest.split_whitespace().skip(3);
    let session = fields.next()?.parse().ok()?;
    let tty_nr = fields.next()?.parse().ok()?;
    Some((session, tty_nr))
}

/// Signal processes by bare pid. Test-only: production code signals through
/// `ProcessHandle`, which cannot hit a reused pid.
#[cfg(test)]
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
pub fn process_exists(pid: u32) -> bool {
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

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

/// How long one clipboard read or write may take, across every helper it
/// tries, before the helper is killed. A helper can hang indefinitely (an X
/// selection owner that never answers, a compositor that is gone) and must
/// not outlive the request that started it.
const CLIPBOARD_HELPER_TIMEOUT: Duration = Duration::from_secs(2);

pub fn write_clipboard(bytes: &[u8]) -> bool {
    write_clipboard_with(&clipboard_commands(ClipboardSession::from_env()), bytes)
}

fn write_clipboard_with(commands: &[ClipboardCommand], bytes: &[u8]) -> bool {
    let deadline = Instant::now() + CLIPBOARD_HELPER_TIMEOUT;
    commands
        .iter()
        .any(|command| run_clipboard_command(command, bytes, deadline))
}

pub fn read_clipboard_text() -> Option<String> {
    let deadline = Instant::now() + CLIPBOARD_HELPER_TIMEOUT;
    read_clipboard_text_commands(ClipboardSession::from_env())
        .iter()
        .find_map(|command| read_clipboard_text_with_command(command, deadline))
}

/// Which display servers the clipboard commands may talk to. Read from the
/// environment once per call and passed in, so the command lists are pure and
/// tests never have to mutate the process environment.
#[derive(Debug, Clone, Copy)]
struct ClipboardSession {
    wayland: bool,
    x11: bool,
}

impl ClipboardSession {
    fn from_env() -> Self {
        Self {
            wayland: std::env::var_os("WAYLAND_DISPLAY").is_some(),
            x11: std::env::var_os("DISPLAY").is_some(),
        }
    }
}

/// The executable's base name, so a command given by absolute path is still
/// recognised (`/usr/bin/wl-copy` is `wl-copy`).
fn clipboard_program_name(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

fn clipboard_commands(session: ClipboardSession) -> Vec<ClipboardCommand> {
    let mut commands = Vec::new();

    if session.wayland {
        commands.push(ClipboardCommand {
            program: "wl-copy",
            args: &["--type", "text/plain;charset=utf-8"],
        });
    }

    if session.x11 {
        commands.push(ClipboardCommand {
            program: "xclip",
            args: &["-selection", "clipboard", "-in"],
        });
        commands.push(ClipboardCommand {
            program: "xsel",
            args: &["--clipboard", "--input"],
        });
    }

    commands
}

fn read_clipboard_text_commands(session: ClipboardSession) -> Vec<ClipboardCommand> {
    let mut commands = Vec::new();

    if session.wayland {
        commands.push(ClipboardCommand {
            program: "wl-paste",
            args: &["--type", "text/plain;charset=utf-8"],
        });
        commands.push(ClipboardCommand {
            program: "wl-paste",
            args: &["--type", "text/plain"],
        });
    }

    if session.x11 {
        commands.push(ClipboardCommand {
            program: "xclip",
            args: &["-selection", "clipboard", "-out"],
        });
        commands.push(ClipboardCommand {
            program: "xsel",
            args: &["--clipboard", "--output"],
        });
    }

    commands
}

/// A reader that fails with `TimedOut` instead of blocking past `deadline`.
struct DeadlineReader<R> {
    inner: R,
    deadline: Instant,
}

impl<R: Read + AsRawFd> Read for DeadlineReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let timed_out = || std::io::Error::from(std::io::ErrorKind::TimedOut);
        let wait_ms = poll_timeout_until(self.deadline).ok_or_else(timed_out)?;
        if !poll_fd_readable(self.inner.as_raw_fd(), wait_ms)? {
            return Err(timed_out());
        }
        self.inner.read(buffer)
    }
}

/// Stop a helper that failed or ran out of time.
fn kill_and_reap(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Wait for `child` until `deadline`, then kill it. `None` on a timeout or a
/// failed wait.
fn wait_child_until(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<std::process::ExitStatus> {
    const POLL_INTERVAL: Duration = Duration::from_millis(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) | Err(_) => {
                kill_and_reap(child);
                return None;
            }
        }
    }
}

/// Write all of `bytes` to a nonblocking pipe, failing with `TimedOut` once
/// `deadline` passes.
fn write_all_until(
    pipe: &mut std::process::ChildStdin,
    mut bytes: &[u8],
    deadline: Instant,
) -> std::io::Result<()> {
    use std::io::ErrorKind;
    while !bytes.is_empty() {
        match pipe.write(bytes) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                let wait_ms = poll_timeout_until(deadline)
                    .ok_or_else(|| std::io::Error::from(ErrorKind::TimedOut))?;
                match poll_fd(pipe.as_raw_fd(), libc::POLLOUT, wait_ms) {
                    Ok(false) => return Err(ErrorKind::TimedOut.into()),
                    Ok(true) => {}
                    Err(error) if error.kind() == ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn read_clipboard_text_with_command(
    command: &ClipboardCommand,
    deadline: Instant,
) -> Option<String> {
    const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024;

    let mut child = Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let Some(stdout) = child.stdout.take() else {
        kill_and_reap(&mut child);
        return None;
    };
    let stdout = DeadlineReader {
        inner: stdout,
        deadline,
    };
    let bytes = match read_limited_reader(stdout, MAX_CLIPBOARD_TEXT_BYTES) {
        Ok(LimitedRead::Complete(bytes)) => Some(bytes),
        Ok(LimitedRead::Empty) => None,
        // Too large, unreadable, or out of time: stop the helper rather than
        // wait for it to finish writing into a pipe nobody reads.
        Ok(LimitedRead::Oversized) | Err(_) => {
            kill_and_reap(&mut child);
            return None;
        }
    };

    let status = wait_child_until(&mut child, deadline)?;
    if !status.success() {
        return None;
    }
    String::from_utf8(bytes?).ok()
}

fn run_clipboard_command(command: &ClipboardCommand, bytes: &[u8], deadline: Instant) -> bool {
    let mut child = match Command::new(command.program)
        .args(command.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    let Some(mut stdin) = child.stdin.take() else {
        kill_and_reap(&mut child);
        return false;
    };

    // Nonblocking, so a helper that stops reading cannot hold this write
    // past the deadline.
    let written = crate::pty::fd::set_nonblocking(stdin.as_raw_fd())
        .and_then(|()| write_all_until(&mut stdin, bytes, deadline));
    if written.is_err() {
        kill_and_reap(&mut child);
        return false;
    }
    drop(stdin);

    if clipboard_program_name(command.program) == "wl-copy" {
        return wait_for_wl_copy_startup(child);
    }

    wait_child_until(&mut child, deadline).is_some_and(|status| status.success())
}

fn wait_for_wl_copy_startup(mut child: std::process::Child) -> bool {
    const STARTUP_WAIT: Duration = Duration::from_millis(100);
    const POLL_INTERVAL: Duration = Duration::from_millis(5);

    let deadline = Instant::now() + STARTUP_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(POLL_INTERVAL);
            }
            Ok(None) => return detach_clipboard_owner(child),
            Err(_) => {
                kill_and_reap(&mut child);
                return false;
            }
        }
    }
}

fn detach_clipboard_owner(child: std::process::Child) -> bool {
    let pid = child.id();
    let child = std::sync::Arc::new(std::sync::Mutex::new(child));
    let reaper_child = std::sync::Arc::clone(&child);
    let reaper = std::thread::Builder::new()
        .name("shepr-wl-copy-reaper".to_string())
        .spawn(move || {
            let wait_result = match reaper_child.lock() {
                Ok(mut child) => child.wait(),
                Err(poisoned) => poisoned.into_inner().wait(),
            };
            if let Err(err) = wait_result {
                tracing::warn!(pid, %err, "failed to reap wl-copy clipboard owner");
            }
        });

    if let Err(err) = reaper {
        tracing::warn!(pid, %err, "failed to start wl-copy clipboard owner reaper");
        let mut child = match child.lock() {
            Ok(child) => child,
            Err(poisoned) => poisoned.into_inner(),
        };
        kill_and_reap(&mut child);
        return false;
    }

    true
}
