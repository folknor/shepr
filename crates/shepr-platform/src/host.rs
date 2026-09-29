use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub fn terminal_grid_size() -> std::io::Result<(u16, u16)> {
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
pub fn watch_terminal_resize_signal() {
    // SAFETY: sigaction is a plain C struct; all-zero is a valid value (no
    // flags, empty mask, SIG_DFL), and the fields that matter are set below.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    let handler: extern "C" fn(libc::c_int) = record_terminal_resize_signal;
    action.sa_sigaction = handler as libc::sighandler_t;
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
pub fn take_terminal_resize_signal() -> bool {
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

pub fn begin_cli_output() {
    set_sigpipe_disposition(libc::SIG_DFL);
}

/// A child process command that runs in `cwd` rather than inheriting this
/// process's working directory. A long-lived child that inherits it pins that
/// directory (an unmount fails with EBUSY, a deleted one stays referenced) and
/// resolves any relative path against wherever shepr happened to start.
///
/// The command never inherits `SHEPR_STARTUP_CWD`: it is a one-time handoff
/// from a client to the server daemon it spawns, which the server reads at
/// launch and leaves in its own environment. The daemon spawn sets it again
/// explicitly on the returned command.
pub fn child_command(program: impl AsRef<std::ffi::OsStr>, cwd: &Path) -> Command {
    #[expect(
        clippy::disallowed_methods,
        reason = "the shared constructor; it states the working directory on the next line"
    )]
    let mut command = Command::new(program);
    command.current_dir(cwd);
    command.env_remove(shepr_core::env::EnvVar::SheprStartupCwd);
    command
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

/// The path to run to start this program again. Use this, never raw
/// `current_exe()`, for anything that re-executes shepr or hands its path to
/// another process: once an install replaces the binary, Linux reports the
/// running one as "/…/shepr (deleted)", a path nothing can execute.
pub fn launch_executable() -> std::io::Result<PathBuf> {
    resolve_launch_executable(std::env::current_exe()?, is_regular_file)
}

/// Whether `path` names a regular file (following symlinks). Absence is
/// `false`; any other stat failure is an error, not absence.
fn is_regular_file(path: &Path) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn resolve_launch_executable(
    executable: PathBuf,
    is_file: impl Fn(&Path) -> std::io::Result<bool>,
) -> std::io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    if !is_file(&executable)? {
        // Linux marks the old inode as deleted after an update replaces the binary.
        if let Some(path) = executable
            .as_os_str()
            .as_bytes()
            .strip_suffix(b" (deleted)")
        {
            let replacement = PathBuf::from(std::ffi::OsStr::from_bytes(path));
            if is_file(&replacement)? {
                return Ok(replacement);
            }
        }
    }
    Ok(executable)
}

/// The machine's node name, as shown by tmux's `#h`.
pub fn hostname() -> Option<String> {
    let mut buffer = [0_u8; super::limits::HOSTNAME_BUFFER_BYTES];
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

pub fn local_datetime() -> Option<time::PrimitiveDateTime> {
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

pub(super) fn effective_uid() -> libc::uid_t {
    // SAFETY: geteuid(2) takes no arguments, cannot fail and touches no memory.
    unsafe { libc::geteuid() }
}
