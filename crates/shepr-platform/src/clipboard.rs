//! Clipboard data may contain credentials or other private text. Never put it
//! in logs or error messages; diagnostics on this path may include byte counts
//! and error kinds only.

use super::*;
use std::{io::Write, os::fd::AsRawFd, process::Stdio, time::Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClipboardCommand {
    pub(super) program: &'static str,
    pub(super) args: &'static [&'static str],
    /// Whether the helper becomes the clipboard owner after reading stdin.
    /// Such helpers stay alive until another process replaces the selection.
    pub(super) owns_selection_after_exit: bool,
}

/// Clipboard helpers read no paths. A selection-owning helper can outlive the
/// request, so it must not pin the directory shepr happened to start in.
fn clipboard_helper_dir() -> &'static std::path::Path {
    std::path::Path::new("/")
}

/// The real clock, in the shared form the child-output deadline reader holds.
fn system_clock() -> std::sync::Arc<dyn Fn() -> Instant + Send + Sync> {
    // clock-io-ok: the public clipboard entry points supply the real clock.
    std::sync::Arc::new(Instant::now)
}

/// How clipboard writes leave this host, decided once from the host's
/// environment: through the host terminal's OSC 52, or through the display
/// server's clipboard helpers. Remote and VS Code remote sessions route through
/// the terminal so bytes reach the user's own machine. Prompt reads use this
/// same route, and return no text when it is OSC 52 because that route has no
/// portable read operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardRoute {
    /// The host terminal carries the bytes; no helper is run.
    Osc52,
    /// Helpers for these display servers take the bytes first.
    Helpers(ClipboardSession),
}

impl ClipboardRoute {
    pub fn from_env() -> Self {
        if crate::terminal_environment::prefers_osc52_clipboard() {
            Self::Osc52
        } else {
            Self::Helpers(ClipboardSession::from_env())
        }
    }

    /// Hands `bytes` to a clipboard helper, and then to a primary selection
    /// helper. `false` means no clipboard helper took them: the route is OSC
    /// 52, no display server is available, or every helper failed, so the
    /// caller falls back to the terminal, which sets both selections itself.
    /// The primary selection is best effort once the clipboard is set.
    pub fn write_with_helpers(self, bytes: &[u8]) -> bool {
        match self {
            Self::Osc52 => false,
            Self::Helpers(session) => write_clipboard_and_primary_with(
                &clipboard_commands(session),
                &primary_selection_commands(session),
                bytes,
            ),
        }
    }
}

/// Hands `bytes` to the first of `clipboard` that takes them, then to the first
/// of `primary` that does, all under the one helper deadline. `false` when no
/// clipboard helper took them; the primary selection is best effort.
pub(super) fn write_clipboard_and_primary_with(
    clipboard: &[ClipboardCommand],
    primary: &[ClipboardCommand],
    bytes: &[u8],
) -> bool {
    let now = system_clock();
    let deadline = now() + super::limits::CLIPBOARD_HELPER_TIMEOUT;
    let (clipboard_set, primary_set) =
        write_selections_until(clipboard, primary, deadline, now.as_ref(), |command| {
            run_clipboard_command_with_clock(command, bytes, deadline, &now)
        });
    if clipboard_set && !primary_set {
        tracing::debug!(bytes = bytes.len(), "no helper set the primary selection");
    }
    clipboard_set
}

/// Runs the first of `clipboard` that `run` reports took the bytes, then the
/// first of `primary` that does, starting no helper once `deadline` has
/// passed: a helper started late would stretch the synchronous copy past its
/// budget. Returns whether the clipboard and the primary selection were set;
/// the primary selection is only tried once the clipboard is.
fn write_selections_until(
    clipboard: &[ClipboardCommand],
    primary: &[ClipboardCommand],
    deadline: Instant,
    now: &dyn Fn() -> Instant,
    mut run: impl FnMut(&ClipboardCommand) -> bool,
) -> (bool, bool) {
    let mut write = |commands: &[ClipboardCommand]| {
        commands
            .iter()
            .any(|command| remaining_until(deadline, now()).is_some() && run(command))
    };
    if !write(clipboard) {
        return (false, false);
    }
    (true, write(primary))
}

/// Reads from the launch-selected helper route, or returns no text when the
/// client routes clipboard writes through OSC 52.
pub fn read_clipboard_text(route: ClipboardRoute) -> Option<String> {
    // Modal paste treats no clipboard text as no insertion. This best-effort
    // API folds an empty selection, no display backend and helper failure
    // together, so warning on `None` would report expected empty/no-display
    // cases as errors. Distinguishing them needs an outcome the client caller
    // can handle.
    read_clipboard_text_with_clock(route, &system_clock())
}

fn read_clipboard_text_with_clock(
    route: ClipboardRoute,
    now: &std::sync::Arc<dyn Fn() -> Instant + Send + Sync>,
) -> Option<String> {
    let ClipboardRoute::Helpers(session) = route else {
        // OSC 52 is a write-only route here: terminals do not provide a
        // portable clipboard read query. In particular, do not read an SSH
        // host's forwarded display variable as though it were the user's clipboard.
        return None;
    };
    let deadline = now() + super::limits::CLIPBOARD_HELPER_TIMEOUT;
    read_clipboard_text_commands(session)
        .iter()
        .find_map(|command| read_clipboard_text_with_command_with_clock(command, deadline, now))
}

/// Which display servers the clipboard commands may talk to. Captured from the
/// environment when the client selects its route, so the command lists are
/// pure and tests never have to mutate the process environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardSession {
    pub(super) wayland: bool,
    pub(super) x11: bool,
}

impl ClipboardSession {
    /// A session with no display server, so no helper is selected.
    pub const fn none() -> Self {
        Self {
            wayland: false,
            x11: false,
        }
    }

    fn from_env() -> Self {
        Self {
            wayland: crate::env_present(shepr_core::env::EnvVar::WaylandDisplay),
            x11: crate::env_present(shepr_core::env::EnvVar::Display),
        }
    }
}

/// Selects Linux clipboard helpers from the current display session. Keep this
/// beside the bounded child and pipe plumbing it uses; moving the selection
/// would leave the platform dependency in place without removing a crate edge.
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

/// The helpers that set the primary selection, the one middle-click pastes,
/// in the same order as [`clipboard_commands`].
pub(super) fn primary_selection_commands(session: ClipboardSession) -> Vec<ClipboardCommand> {
    let mut commands = Vec::new();

    if session.wayland {
        commands.push(ClipboardCommand {
            program: "wl-copy",
            args: &["--primary", "--type", "text/plain;charset=utf-8"],
            owns_selection_after_exit: true,
        });
    }

    if session.x11 {
        commands.push(ClipboardCommand {
            program: "xclip",
            args: &["-selection", "primary", "-in"],
            owns_selection_after_exit: false,
        });
        commands.push(ClipboardCommand {
            program: "xsel",
            args: &["--primary", "--input"],
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
            args: &["--no-newline", "--type", "text/plain;charset=utf-8"],
            owns_selection_after_exit: false,
        });
        commands.push(ClipboardCommand {
            program: "wl-paste",
            args: &["--no-newline", "--type", "text/plain"],
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
        crate::structured_log!(
            WARN, event = clipboard.helper_kill, outcome = Error,
            pid,
            error_kind = ?err.kind(),
            "failed to kill clipboard helper"
        );
    }
    if let Err(err) = child.wait() {
        crate::structured_log!(
            WARN, event = clipboard.helper_reap, outcome = Error,
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
    now: &dyn Fn() -> Instant,
) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if now() < deadline => {
                std::thread::sleep(super::limits::HELPER_PROCESS_POLL_INTERVAL);
            }
            Ok(None) | Err(_) => {
                kill_and_reap(child);
                return None;
            }
        }
    }
}

/// Write all of `bytes` to a nonblocking pipe, failing with `TimedOut` once
/// `deadline` passes. The deadline is checked before every write, not only
/// when the pipe is full, so a helper that keeps reading slowly cannot hold
/// the write past it either.
fn write_all_until(
    pipe: &mut std::process::ChildStdin,
    mut bytes: &[u8],
    deadline: Instant,
    now: &dyn Fn() -> Instant,
) -> std::io::Result<()> {
    use std::io::ErrorKind;
    while !bytes.is_empty() {
        if remaining_until(deadline, now()).is_none() {
            return Err(ErrorKind::TimedOut.into());
        }
        match pipe.write(bytes) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                let remaining = remaining_until(deadline, now())
                    .ok_or_else(|| std::io::Error::from(ErrorKind::TimedOut))?;
                match poll_fd(pipe.as_raw_fd(), libc::POLLOUT, remaining) {
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

pub(super) fn read_clipboard_text_with_command_with_clock(
    command: &ClipboardCommand,
    deadline: Instant,
    now: &std::sync::Arc<dyn Fn() -> Instant + Send + Sync>,
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
    let stdout = super::child_io::DeadlineReader::new_with_clock(
        stdout,
        deadline,
        std::sync::Arc::clone(now),
    );
    let bytes = match read_limited_reader(stdout, super::limits::MAX_CLIPBOARD_TEXT_BYTES) {
        Ok(LimitedRead::Complete(bytes)) => Some(bytes),
        Ok(LimitedRead::Empty) => None,
        // Too large, unreadable, or out of time: stop the helper rather than
        // wait for it to finish writing into a pipe nobody reads.
        Ok(LimitedRead::Oversized) | Err(_) => {
            kill_and_reap(&mut child);
            return None;
        }
    };

    let status = wait_child_until(&mut child, deadline, now.as_ref())?;
    if !status.success() {
        return None;
    }
    String::from_utf8(bytes?).ok()
}

pub(super) fn run_clipboard_command_with_clock(
    command: &ClipboardCommand,
    bytes: &[u8],
    deadline: Instant,
    now: &std::sync::Arc<dyn Fn() -> Instant + Send + Sync>,
) -> bool {
    // A helper started with no time left could only be killed again, after
    // stretching the copy past its budget.
    if remaining_until(deadline, now()).is_none() {
        return false;
    }
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
        .and_then(|()| write_all_until(&mut stdin, bytes, deadline, now.as_ref()));
    if written.is_err() {
        kill_and_reap(&mut child);
        return false;
    }
    drop(stdin);

    if command.owns_selection_after_exit {
        return wait_for_selection_owner_startup(child, deadline, now.as_ref());
    }

    wait_child_until(&mut child, deadline, now.as_ref()).is_some_and(|status| status.success())
}

/// Waits for a selection-owning helper to either exit (its status decides) or
/// keep running (it owns the selection and is detached), for its startup wait
/// or until the copy's shared `budget` ends, whichever comes first.
fn wait_for_selection_owner_startup(
    mut child: std::process::Child,
    budget: Instant,
    now: &dyn Fn() -> Instant,
) -> bool {
    let deadline = (now() + super::limits::CLIPBOARD_OWNER_STARTUP_WAIT).min(budget);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if now() < deadline => {
                std::thread::sleep(super::limits::HELPER_PROCESS_POLL_INTERVAL);
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
        // Linux exposes at most 15 bytes through `pthread_setname_np`.
        .name("clip-owner-reap".to_string())
        .spawn(move || {
            let wait_result = match reaper_child.lock() {
                Ok(mut child) => child.wait(),
                Err(poisoned) => poisoned.into_inner().wait(),
            };
            if let Err(err) = wait_result {
                crate::structured_log!(
                    WARN, event = clipboard.owner_reap, outcome = Error,
                    pid,
                    error_kind = ?err.kind(),
                    "failed to reap clipboard selection owner"
                );
            }
        });

    if let Err(err) = reaper {
        crate::structured_log!(
            WARN, event = clipboard.reaper_start, outcome = Error,
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

use super::set_nonblocking;

#[cfg(test)]
pub(super) fn read_clipboard_text_with_command(
    command: &ClipboardCommand,
    deadline: Instant,
) -> Option<String> {
    read_clipboard_text_with_command_with_clock(command, deadline, &system_clock())
}

#[cfg(test)]
pub(super) fn run_clipboard_command(
    command: &ClipboardCommand,
    bytes: &[u8],
    deadline: Instant,
) -> bool {
    run_clipboard_command_with_clock(command, bytes, deadline, &system_clock())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::fixture::{self, Step};
    use std::time::Duration;

    #[test]
    fn osc52_route_does_not_read_display_clipboard_helpers() {
        assert_eq!(
            read_clipboard_text_with_clock(ClipboardRoute::Osc52, &system_clock()),
            None
        );
    }

    /// Every clipboard helper has a primary selection counterpart, in the same
    /// order, and the Wayland one keeps owning the selection like its
    /// clipboard twin.
    #[test]
    fn primary_selection_helpers_mirror_the_clipboard_helpers() {
        let session = ClipboardSession {
            wayland: true,
            x11: true,
        };
        let clipboard = clipboard_commands(session);
        let primary = primary_selection_commands(session);
        assert_eq!(
            primary
                .iter()
                .map(|command| (command.program, command.owns_selection_after_exit))
                .collect::<Vec<_>>(),
            clipboard
                .iter()
                .map(|command| (command.program, command.owns_selection_after_exit))
                .collect::<Vec<_>>()
        );
        for command in &primary {
            assert!(
                command
                    .args
                    .iter()
                    .any(|arg| *arg == "--primary" || *arg == "primary"),
                "{command:?}"
            );
        }
        assert!(primary_selection_commands(ClipboardSession::none()).is_empty());
    }

    /// A helper still running when the injected clock reaches the startup
    /// wait is detached as the selection owner, on that very reading.
    #[test]
    fn selection_owner_is_detached_when_the_injected_clock_ends_the_startup_wait() {
        let child = fixture::command(&[Step::Sleep(Duration::from_secs(30))])
            .spawn()
            .expect("test precondition");
        let pid = libc::pid_t::try_from(child.id()).expect("test precondition");
        let started = Instant::now();
        let reads = std::cell::Cell::new(0_u32);
        let now = || {
            let read = reads.get();
            reads.set(read + 1);
            started + super::super::limits::CLIPBOARD_OWNER_STARTUP_WAIT * read
        };

        let budget = started + super::super::limits::CLIPBOARD_OWNER_STARTUP_WAIT * 10;
        let detached = wait_for_selection_owner_startup(child, budget, &now);
        // The reaper thread owns the child now; end it so the reaper returns.
        // SAFETY: kill(2) on the pid of the helper this test spawned, which
        // the detached reaper has not reaped while it still runs.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }

        assert!(detached, "a live helper at the deadline owns the selection");
        assert_eq!(reads.get(), 2, "the wait ends on the read at the deadline");
    }

    /// The owner startup wait ends at the copy's shared deadline when that
    /// comes before the wait's own end.
    #[test]
    fn selection_owner_startup_wait_ends_at_the_shared_deadline() {
        let child = fixture::command(&[Step::Sleep(Duration::from_secs(30))])
            .spawn()
            .expect("test precondition");
        let pid = libc::pid_t::try_from(child.id()).expect("test precondition");
        let started = Instant::now();
        let step = super::super::limits::CLIPBOARD_OWNER_STARTUP_WAIT / 4;
        let reads = std::cell::Cell::new(0_u32);
        let now = || {
            let read = reads.get();
            reads.set(read + 1);
            started + step * read
        };

        // Its own wait would end on the fifth read; the budget ends on the
        // third.
        let detached = wait_for_selection_owner_startup(child, started + step * 2, &now);
        // SAFETY: kill(2) on the pid of the helper this test spawned, which
        // the detached reaper has not reaped while it still runs.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }

        assert!(detached, "a live helper at the deadline owns the selection");
        assert_eq!(reads.get(), 3, "the wait ends on the read at the budget");
    }

    fn both_sessions() -> (Vec<ClipboardCommand>, Vec<ClipboardCommand>) {
        let session = ClipboardSession {
            wayland: true,
            x11: true,
        };
        (
            clipboard_commands(session),
            primary_selection_commands(session),
        )
    }

    /// A clipboard helper that takes the whole budget leaves no time for the
    /// primary selection, so no primary helper is started after it.
    #[test]
    fn no_primary_helper_starts_once_the_clipboard_used_the_budget() {
        let (clipboard, primary) = both_sessions();
        let started = Instant::now();
        let deadline = started + super::super::limits::CLIPBOARD_HELPER_TIMEOUT;
        let clock = std::cell::Cell::new(started);
        let now = || clock.get();
        let mut ran = Vec::new();

        let written = write_selections_until(&clipboard, &primary, deadline, &now, |command| {
            ran.push(command.args);
            clock.set(deadline);
            true
        });

        assert_eq!(written, (true, false));
        assert_eq!(ran, [clipboard[0].args]);
    }

    /// A clipboard helper that fails at the deadline is the last one tried.
    #[test]
    fn no_fallback_clipboard_helper_starts_after_the_deadline() {
        let (clipboard, primary) = both_sessions();
        let started = Instant::now();
        let deadline = started + super::super::limits::CLIPBOARD_HELPER_TIMEOUT;
        let clock = std::cell::Cell::new(started);
        let now = || clock.get();
        let mut ran = 0;

        let written = write_selections_until(&clipboard, &primary, deadline, &now, |_| {
            ran += 1;
            clock.set(deadline);
            false
        });

        assert_eq!(written, (false, false));
        assert_eq!(ran, 1);
    }

    /// Within the budget, the clipboard and then the primary selection each
    /// take the first helper that succeeds, under the one shared deadline.
    #[test]
    fn clipboard_and_primary_share_one_budget() {
        let (clipboard, primary) = both_sessions();
        let started = Instant::now();
        let deadline = started + super::super::limits::CLIPBOARD_HELPER_TIMEOUT;
        let step = super::super::limits::CLIPBOARD_HELPER_TIMEOUT / 4;
        let clock = std::cell::Cell::new(started);
        let now = || clock.get();
        let mut ran = Vec::new();

        // The first helper of each list fails and the second succeeds, each
        // taking a quarter of the budget.
        let written = write_selections_until(&clipboard, &primary, deadline, &now, |command| {
            ran.push(command.args);
            clock.set(clock.get() + step);
            ran.len() % 2 == 0
        });

        assert_eq!(written, (true, true));
        assert_eq!(
            ran,
            [
                clipboard[0].args,
                clipboard[1].args,
                primary[0].args,
                primary[1].args
            ]
        );

        // One more quarter would have left the second primary helper no time.
        let clock = std::cell::Cell::new(started + step);
        let now = || clock.get();
        let mut ran = 0;
        let written = write_selections_until(&clipboard, &primary, deadline, &now, |_| {
            ran += 1;
            clock.set(clock.get() + step);
            ran % 2 == 0
        });
        assert_eq!(written, (true, false));
        assert_eq!(ran, 3);
    }
}
