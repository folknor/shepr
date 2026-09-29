use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) fn set_cloexec(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: F_GETFD/F_SETFD take and return integers and touch no memory;
    // a bad fd fails with EBADF.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: F_GETFL/F_SETFL take and return integers and touch no memory;
    // a bad fd fails with EBADF.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct WakeWriter {
    fd: Arc<OwnedFd>,
}

impl WakeWriter {
    pub(crate) fn wake(&self) -> std::io::Result<()> {
        loop {
            let byte = [1u8];
            // SAFETY: writes `byte.len()` bytes from a live stack array to an
            // fd the `Arc<OwnedFd>` keeps open for the call.
            let written =
                unsafe { libc::write(self.fd.as_raw_fd(), byte.as_ptr().cast(), byte.len()) };
            if written >= 0 {
                return Ok(());
            }

            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                // A full pipe already contains a readable wake byte.
                return Ok(());
            }
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

pub(crate) struct WakePipe {
    pub(crate) read_fd: OwnedFd,
    pub(crate) writer: WakeWriter,
}

pub(crate) fn create_wake_pipe() -> std::io::Result<WakePipe> {
    let mut fds = [-1; 2];
    // SAFETY: pipe2(2) writes exactly two fds into the two-element array and
    // sets both descriptor flags atomically before another process can spawn.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: pipe succeeded, so both are fresh fds nothing else owns; each
    // is wrapped exactly once.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: as above.
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    Ok(WakePipe {
        read_fd,
        writer: WakeWriter {
            fd: Arc::new(write_fd),
        },
    })
}

pub(crate) fn drain_wake_fd(fd: RawFd) -> std::io::Result<()> {
    let mut buf = [0u8; 64];
    loop {
        // SAFETY: reads at most `buf.len()` bytes into a live stack buffer.
        let read = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if read == 0 {
            return Ok(());
        }
        if read > 0 {
            continue;
        }

        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(());
        }
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

#[derive(Default)]
pub(crate) struct PtyWakeReadiness {
    pub(crate) pty_read_ready: bool,
    pub(crate) pty_write_ready: bool,
    pub(crate) pty_error: bool,
    pub(crate) wake_ready: bool,
}

pub(crate) fn poll_pty_and_wake(
    pty_fd: RawFd,
    wake_fd: RawFd,
    poll_pty_write: bool,
    timeout_ms: i32,
) -> std::io::Result<PtyWakeReadiness> {
    let mut pty_events = libc::POLLIN;
    if poll_pty_write {
        pty_events |= libc::POLLOUT;
    }

    let mut poll_fds = [
        libc::pollfd {
            fd: pty_fd,
            events: pty_events,
            revents: 0,
        },
        libc::pollfd {
            fd: wake_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];

    // clock-io-ok: an EINTR retry resumes the kernel wait from the real time
    // the poll began.
    let deadline = (timeout_ms >= 0)
        .then(|| Instant::now() + Duration::from_millis(u64::try_from(timeout_ms).unwrap_or(0)));
    let mut remaining_timeout_ms = timeout_ms;
    loop {
        for poll_fd in &mut poll_fds {
            poll_fd.revents = 0;
        }
        // SAFETY: `poll_fds` is a live two-element pollfd array and the
        // count passed is its length.
        let result = unsafe {
            libc::poll(
                poll_fds.as_mut_ptr(),
                poll_fds.len() as _,
                remaining_timeout_ms,
            )
        };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                let Some(deadline) = deadline else {
                    continue;
                };
                // clock-io-ok: the interrupted poll consumed real time.
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(PtyWakeReadiness::default());
                }
                remaining_timeout_ms =
                    i32::try_from(remaining.as_millis().clamp(1, i32::MAX as u128))
                        .unwrap_or(i32::MAX);
                continue;
            }
            return Err(err);
        }

        let pty_revents = poll_fds[0].revents;
        let wake_revents = poll_fds[1].revents;
        if (pty_revents | wake_revents) & libc::POLLNVAL != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "poll encountered invalid PTY actor fd",
            ));
        }
        return Ok(PtyWakeReadiness {
            pty_read_ready: pty_revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0,
            pty_write_ready: pty_revents & (libc::POLLOUT | libc::POLLHUP) != 0,
            pty_error: pty_revents & libc::POLLERR != 0,
            wake_ready: wake_revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0,
        });
    }
}

pub(crate) fn resize_pty_fd(
    fd: RawFd,
    geometry: shepr_core::geometry::PaneGeometry,
) -> std::io::Result<()> {
    let geometry = geometry.clamped();
    let (pixel_width, pixel_height) = geometry.text_area_px().unwrap_or((0, 0));
    let size = libc::winsize {
        ws_row: geometry.rows(),
        ws_col: geometry.cols(),
        ws_xpixel: pixel_width,
        ws_ypixel: pixel_height,
    };
    // SAFETY: TIOCSWINSZ reads one winsize from `size`, a live local.
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
