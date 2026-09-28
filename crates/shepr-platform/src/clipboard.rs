//! Clipboard data may contain credentials or other private text. Never put it
//! in logs or error messages; diagnostics on this path may include byte counts
//! and error kinds only.

use super::*;
use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    process::Stdio,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClipboardCommand {
    pub(super) program: &'static str,
    pub(super) args: &'static [&'static str],
    /// Whether the helper becomes the clipboard owner after reading stdin.
    /// Such helpers stay alive until another process replaces the selection.
    pub(super) owns_selection_after_exit: bool,
}

/// How long one clipboard read or write may take, across every helper it
/// tries, before the helper is killed. A helper can hang indefinitely (an X
/// selection owner that never answers, a compositor that is gone) and must
/// not outlive the request that started it.
pub(super) const CLIPBOARD_HELPER_TIMEOUT: Duration = Duration::from_secs(2);

/// Clipboard helpers read no paths. A selection-owning helper can outlive the
/// request, so it must not pin the directory shepr happened to start in.
fn clipboard_helper_dir() -> &'static std::path::Path {
    std::path::Path::new("/")
}

/// Maximum bytes read from a host clipboard helper for a paste. This bounds
/// host-initiated reads; the terminal emulator separately bounds terminal-
/// originated OSC 52 stores, which travel in the opposite direction.
pub(super) const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024;

pub fn write_clipboard(bytes: &[u8]) -> bool {
    write_clipboard_with(&clipboard_commands(ClipboardSession::from_env()), bytes)
}

pub(super) fn write_clipboard_with(commands: &[ClipboardCommand], bytes: &[u8]) -> bool {
    let deadline = Instant::now() + CLIPBOARD_HELPER_TIMEOUT;
    commands
        .iter()
        .any(|command| run_clipboard_command(command, bytes, deadline))
}

pub fn read_clipboard_text() -> Option<String> {
    // Modal paste treats no clipboard text as no insertion. This best-effort
    // API folds an empty selection, no display backend and helper failure
    // together, so warning on `None` would report expected empty/no-display
    // cases as errors. Distinguishing them needs an outcome the client caller
    // can handle.
    let deadline = Instant::now() + CLIPBOARD_HELPER_TIMEOUT;
    read_clipboard_text_commands(ClipboardSession::from_env())
        .iter()
        .find_map(|command| read_clipboard_text_with_command(command, deadline))
}

/// Which display servers the clipboard commands may talk to. Read from the
/// environment once per call and passed in, so the command lists are pure and
/// tests never have to mutate the process environment.
#[derive(Debug, Clone, Copy)]
pub(super) struct ClipboardSession {
    pub(super) wayland: bool,
    pub(super) x11: bool,
}

impl ClipboardSession {
    fn from_env() -> Self {
        Self {
            wayland: crate::env_present(shepr_core::env::EnvVar::WaylandDisplay),
            x11: crate::env_present(shepr_core::env::EnvVar::Display),
        }
    }
}

pub(super) fn clipboard_commands(session: ClipboardSession) -> Vec<ClipboardCommand> {
    let mut commands = Vec::new();

    if session.wayland {
        commands.push(ClipboardCommand {
            program: "wl-copy",
            args: &["--type", "text/plain;charset=utf-8"],
            owns_selection_after_exit: true,
        });
    }

    if session.x11 {
        commands.push(ClipboardCommand {
            program: "xclip",
            args: &["-selection", "clipboard", "-in"],
            owns_selection_after_exit: false,
        });
        commands.push(ClipboardCommand {
            program: "xsel",
            args: &["--clipboard", "--input"],
            owns_selection_after_exit: false,
        });
    }

    commands
}

pub(super) fn read_clipboard_text_commands(session: ClipboardSession) -> Vec<ClipboardCommand> {
    let mut commands = Vec::new();

    if session.wayland {
        commands.push(ClipboardCommand {
            program: "wl-paste",
            args: &["--type", "text/plain;charset=utf-8"],
            owns_selection_after_exit: false,
        });
        commands.push(ClipboardCommand {
            program: "wl-paste",
            args: &["--type", "text/plain"],
            owns_selection_after_exit: false,
        });
    }

    if session.x11 {
        commands.push(ClipboardCommand {
            program: "xclip",
            args: &["-selection", "clipboard", "-out"],
            owns_selection_after_exit: false,
        });
        commands.push(ClipboardCommand {
            program: "xsel",
            args: &["--clipboard", "--output"],
            owns_selection_after_exit: false,
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
///
/// The clipboard request has already failed by the time this runs, so the
/// caller has nothing to add; a helper that cannot be killed or reaped is left
/// running or as a zombie, which is logged by pid.
fn kill_and_reap(child: &mut std::process::Child) {
    let pid = child.id();
    // `kill` succeeds on a helper that already exited but is not yet reaped,
    // so an error here means the signal did not reach it.
    if let Err(err) = child.kill() {
        tracing::warn!(
            pid,
            error_kind = ?err.kind(),
            "failed to kill clipboard helper"
        );
    }
    if let Err(err) = child.wait() {
        tracing::warn!(
            pid,
            error_kind = ?err.kind(),
            "failed to reap clipboard helper"
        );
    }
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

pub(super) fn read_clipboard_text_with_command(
    command: &ClipboardCommand,
    deadline: Instant,
) -> Option<String> {
    let mut child = child_command(command.program, clipboard_helper_dir())
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

pub(super) fn run_clipboard_command(
    command: &ClipboardCommand,
    bytes: &[u8],
    deadline: Instant,
) -> bool {
    let mut child = match child_command(command.program, clipboard_helper_dir())
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
    let written = set_nonblocking(stdin.as_raw_fd())
        .and_then(|()| write_all_until(&mut stdin, bytes, deadline));
    if written.is_err() {
        kill_and_reap(&mut child);
        return false;
    }
    drop(stdin);

    if command.owns_selection_after_exit {
        return wait_for_selection_owner_startup(child);
    }

    wait_child_until(&mut child, deadline).is_some_and(|status| status.success())
}

fn wait_for_selection_owner_startup(mut child: std::process::Child) -> bool {
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
        .name("shepr-clipboard-owner-reaper".to_string())
        .spawn(move || {
            let wait_result = match reaper_child.lock() {
                Ok(mut child) => child.wait(),
                Err(poisoned) => poisoned.into_inner().wait(),
            };
            if let Err(err) = wait_result {
                tracing::warn!(
                    pid,
                    error_kind = ?err.kind(),
                    "failed to reap clipboard selection owner"
                );
            }
        });

    if let Err(err) = reaper {
        tracing::warn!(
            pid,
            error_kind = ?err.kind(),
            "failed to start clipboard owner reaper"
        );
        let mut child = match child.lock() {
            Ok(child) => child,
            Err(poisoned) => poisoned.into_inner(),
        };
        kill_and_reap(&mut child);
        return false;
    }

    true
}

fn set_nonblocking(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl only inspects or updates flags on the borrowed live fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd remains open for this call and F_SETFL receives flag bits.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
