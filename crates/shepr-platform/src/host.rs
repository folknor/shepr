use super::*;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
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

pub(super) fn is_detached_session(pid: u32, session: i32, tty_nr: i32) -> bool {
    i64::from(session) == i64::from(pid) && tty_nr == 0
}

/// The path to run to start this program again. Use this, never raw
/// `current_exe()`, for anything that re-executes shepr or hands its path to
/// another process: once an install replaces the binary, Linux reports the
/// running one as "/…/shepr (deleted)", a path nothing can execute.
pub fn launch_executable() -> std::io::Result<PathBuf> {
    Ok(resolve_launch_executable(
        std::env::current_exe()?,
        Path::is_file,
    ))
}

pub(super) fn resolve_launch_executable(
    executable: PathBuf,
    is_file: impl Fn(&Path) -> bool,
) -> PathBuf {
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

pub fn should_draw_host_cursor_by_default() -> bool {
    running_inside_wsl()
}

pub fn should_query_host_terminal_palette() -> bool {
    !running_inside_wsl()
}

pub fn running_inside_wsl() -> bool {
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

pub(super) fn text_indicates_wsl(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("microsoft") || text.contains("wsl")
}

/// The machine's node name, as shown by tmux's `#h`.
pub fn hostname() -> Option<String> {
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
