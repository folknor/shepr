use super::*;
use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClipboardCommand {
    pub(super) program: &'static str,
    pub(super) args: &'static [&'static str],
}

/// How long one clipboard read or write may take, across every helper it
/// tries, before the helper is killed. A helper can hang indefinitely (an X
/// selection owner that never answers, a compositor that is gone) and must
/// not outlive the request that started it.
pub(super) const CLIPBOARD_HELPER_TIMEOUT: Duration = Duration::from_secs(2);

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
            wayland: std::env::var_os("WAYLAND_DISPLAY").is_some(),
            x11: std::env::var_os("DISPLAY").is_some(),
        }
    }
}

/// The executable's base name, so a command given by absolute path is still
/// recognised (`/usr/bin/wl-copy` is `wl-copy`).
pub(super) fn clipboard_program_name(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

pub(super) fn clipboard_commands(session: ClipboardSession) -> Vec<ClipboardCommand> {
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

pub(super) fn read_clipboard_text_commands(session: ClipboardSession) -> Vec<ClipboardCommand> {
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

pub(super) fn read_clipboard_text_with_command(
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

pub(super) fn run_clipboard_command(
    command: &ClipboardCommand,
    bytes: &[u8],
    deadline: Instant,
) -> bool {
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
