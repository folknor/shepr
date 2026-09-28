use std::{
    io::{self, Read},
    os::fd::{AsRawFd, RawFd},
    time::Instant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExitReason {
    Exited,
    Interrupted,
    WaitFailed,
    /// The pane's PTY reader panicked (or found the terminal core poisoned).
    /// The child may still be running; the pane is ended so its session is
    /// torn down, and no checkpoint is taken because the core is broken.
    ReaderPanicked,
    /// The PTY actor hit a hard poll or wake-pipe failure and can no longer
    /// read the pane. Its terminal core remains usable, so checkpoint the
    /// current pane state before removing it and tearing down its session.
    ReaderIoFailed,
}

impl ChildExitReason {
    /// A signal exit or a reader IO failure with an intact terminal core needs
    /// a final session checkpoint before pane removal. shepr-generated
    /// teardown signals follow pane removal, or happen during startup failure
    /// before any pane exit event, so they cannot skip a checkpoint for a pane
    /// that is still live. A reader panic skips it because the core is broken.
    pub fn requires_session_checkpoint(self) -> bool {
        matches!(self, Self::Interrupted | Self::ReaderIoFailed)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum LimitedRead {
    Empty,
    Complete(Vec<u8>),
    Oversized,
}

/// Classify an exit status returned by a successful child wait. Wait errors
/// are classified as `WaitFailed` by the child-watcher call site.
pub fn classify_child_exit(status: &std::process::ExitStatus) -> ChildExitReason {
    use std::os::unix::process::ExitStatusExt;

    if status.signal().is_some() {
        ChildExitReason::Interrupted
    } else {
        ChildExitReason::Exited
    }
}

pub fn read_fd(fd: RawFd, data: &mut [u8]) -> std::io::Result<usize> {
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
pub(super) fn poll_fd(fd: RawFd, events: libc::c_short, timeout_ms: i32) -> std::io::Result<bool> {
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

pub fn poll_fd_readable(fd: RawFd, timeout_ms: i32) -> std::io::Result<bool> {
    poll_fd(fd, libc::POLLIN, timeout_ms)
}

/// A reader that waits for fd readiness only until one overall deadline.
///
/// The wrapped read happens only after `poll(2)` reports the fd ready, so a
/// blocking stream cannot restart an idle timeout after each successful byte.
pub(super) struct DeadlineReader<R> {
    inner: R,
    deadline: Instant,
}

impl<R> DeadlineReader<R> {
    pub(super) fn new(inner: R, deadline: Instant) -> Self {
        Self { inner, deadline }
    }
}

impl<R: Read + AsRawFd> Read for DeadlineReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let wait_ms = poll_timeout_until(self.deadline)
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
        if !poll_fd_readable(self.inner.as_raw_fd(), wait_ms)? {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        self.inner.read(buffer)
    }
}

/// Milliseconds left until `deadline` as a poll timeout, at least 1 so a
/// wait that is nearly due still sleeps instead of spinning. `None` once the
/// deadline has passed.
pub(super) fn poll_timeout_until(deadline: Instant) -> Option<i32> {
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

#[cfg(test)]
mod tests {
    use super::ChildExitReason;

    #[test]
    fn reader_failure_checkpoint_policy_distinguishes_a_broken_core() {
        assert!(ChildExitReason::ReaderIoFailed.requires_session_checkpoint());
        assert!(!ChildExitReason::ReaderPanicked.requires_session_checkpoint());
    }
}
