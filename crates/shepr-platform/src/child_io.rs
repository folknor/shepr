use std::{
    io::{self, Read},
    os::fd::{AsRawFd, RawFd},
    sync::Arc,
    time::{Duration, Instant},
};

/// How a reaped child ended. What that means for the pane, and whether it
/// needs a session checkpoint, is mux's `PaneEnding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExitKind {
    /// It exited with a status code.
    Exited,
    /// A signal ended it.
    Signalled,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum LimitedRead {
    Empty,
    Complete(Vec<u8>),
    Oversized,
}

/// Classify an exit status returned by a successful child wait.
pub fn classify_child_exit(status: &std::process::ExitStatus) -> ChildExitKind {
    use std::os::unix::process::ExitStatusExt;

    if status.signal().is_some() {
        ChildExitKind::Signalled
    } else {
        ChildExitKind::Exited
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

/// How long a poll may block. The only place this becomes poll(2)'s `int`
/// milliseconds is [`Wait::poll_millis`], at the libc call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Until the fd is ready, however long that takes.
    Forever,
    /// Up to this long.
    After(Duration),
    /// Do not block: report what is ready now.
    Now,
}

impl From<Duration> for Wait {
    fn from(duration: Duration) -> Self {
        if duration.is_zero() {
            Self::Now
        } else {
            Self::After(duration)
        }
    }
}

impl Wait {
    /// The poll(2) timeout for this wait: -1 forever, 0 now, otherwise whole
    /// milliseconds rounded down but at least 1, so a wait that is nearly due
    /// still sleeps instead of spinning.
    pub fn poll_millis(self) -> i32 {
        match self {
            Self::Forever => -1,
            Self::Now => 0,
            Self::After(duration) if duration.is_zero() => 0,
            Self::After(duration) => i32::try_from(
                duration
                    .as_millis()
                    .max(super::limits::MIN_POLL_TIMEOUT_MILLISECONDS),
            )
            .unwrap_or(i32::MAX),
        }
    }
}

/// Wait up to `wait` for `events` on `fd`. True when poll reported anything,
/// including POLLHUP/POLLERR, which the next read or write then turns into EOF
/// or an error.
pub(super) fn poll_fd(
    fd: RawFd,
    events: libc::c_short,
    wait: impl Into<Wait>,
) -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: one pollfd that lives on this stack frame for the call.
    let result = unsafe { libc::poll(&mut descriptor, 1, wait.into().poll_millis()) };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(result > 0)
    }
}

pub fn poll_fd_readable(fd: RawFd, wait: impl Into<Wait>) -> std::io::Result<bool> {
    poll_fd(fd, libc::POLLIN, wait)
}

/// A reader that waits for fd readiness only until one overall deadline.
///
/// The wrapped read happens only after `poll(2)` reports the fd ready, so a
/// blocking stream cannot restart an idle timeout after each successful byte.
pub(super) struct DeadlineReader<R> {
    inner: R,
    deadline: Instant,
    now: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl<R> DeadlineReader<R> {
    pub(super) fn new_with_clock(
        inner: R,
        deadline: Instant,
        now: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        Self {
            inner,
            deadline,
            now,
        }
    }
}

impl<R: Read + AsRawFd> Read for DeadlineReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = remaining_until(self.deadline, (self.now)())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
        if !poll_fd_readable(self.inner.as_raw_fd(), remaining)? {
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        self.inner.read(buffer)
    }
}

/// Time left until `deadline`, or `None` once it has passed.
pub fn remaining_until(deadline: Instant, now: Instant) -> Option<Duration> {
    let remaining = deadline.saturating_duration_since(now);
    (!remaining.is_zero()).then_some(remaining)
}

pub(crate) fn read_limited_reader(
    mut reader: impl Read,
    max_bytes: usize,
) -> std::io::Result<LimitedRead> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; super::limits::LIMITED_READ_BUFFER_BYTES];

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

    let mut sentinel = [0_u8; super::limits::LIMITED_READ_OVERFLOW_PROBE_BYTES];
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

pub fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    set_fd_nonblocking(fd, true)
}

/// Change only O_NONBLOCK, preserving the descriptor's other status flags.
pub fn set_fd_nonblocking(fd: RawFd, nonblocking: bool) -> std::io::Result<()> {
    // SAFETY: F_GETFL/F_SETFL take and return integers and touch no memory;
    // a bad fd fails with EBADF.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    let flags = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Wait, remaining_until};
    use std::time::{Duration, Instant};

    #[test]
    fn poll_timeout_uses_the_supplied_clock_at_the_deadline() {
        let start = Instant::now();
        let deadline = start + Duration::from_millis(300);

        assert_eq!(
            remaining_until(deadline, start),
            Some(Duration::from_millis(300))
        );
        assert_eq!(remaining_until(deadline, deadline), None);
        assert_eq!(
            remaining_until(deadline, deadline + Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn wait_converts_to_poll_millis_only_at_the_edge() {
        assert_eq!(Wait::Forever.poll_millis(), -1);
        assert_eq!(Wait::Now.poll_millis(), 0);
        assert_eq!(Wait::from(Duration::ZERO), Wait::Now);
        assert_eq!(Wait::from(Duration::from_micros(10)).poll_millis(), 1);
        assert_eq!(Wait::from(Duration::from_millis(300)).poll_millis(), 300);
        assert_eq!(
            Wait::from(Duration::from_secs(u64::MAX)).poll_millis(),
            i32::MAX
        );
    }
}
