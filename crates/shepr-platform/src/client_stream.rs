use super::*;
use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

fn shutdown_client_stream(stream: &crate::ipc::LocalStream) -> std::io::Result<()> {
    stream.shutdown(std::net::Shutdown::Both)
}

pub struct ClientStreamReader<'a>(pub &'a mut crate::ipc::LocalStream);

impl Read for ClientStreamReader<'_> {
    fn read(&mut self, data: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.0.read(data) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    // Sleep until input or shutdown, without polling quiet observers.
                    if let Err(error) = poll_fd_readable(self.0.as_raw_fd(), Wait::Forever)
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

/// Writes bytes to a client stream, disconnecting a peer after `stall_timeout`
/// without successful write progress.
///
/// The function switches the socket to nonblocking mode and waits for
/// writability between partial writes, so the bound does not depend on socket
/// timeout options configured by the caller. The socket remains nonblocking
/// after the call; readers must handle `WouldBlock`.
pub fn write_client_stream(
    stream: &crate::ipc::LocalStream,
    data: &[u8],
    stall_timeout: Duration,
) -> std::io::Result<()> {
    // clock-io-ok: the public entry point supplies the real clock.
    write_client_stream_with_clock(stream, data, stall_timeout, &Instant::now)
}

fn write_client_stream_with_clock(
    stream: &crate::ipc::LocalStream,
    mut data: &[u8],
    stall_timeout: Duration,
    now: &dyn Fn() -> Instant,
) -> std::io::Result<()> {
    use std::io;

    let mut socket = stream;
    socket.set_nonblocking(true)?;
    let timed_out = || {
        // Dropping the writer clone alone would leave the reader blocked.
        // NotConnected means the peer already hung up, which wakes the reader
        // by itself; any other failure can leave it blocked.
        if let Err(error) = shutdown_client_stream(stream)
            && error.kind() != io::ErrorKind::NotConnected
        {
            tracing::warn!(
                error = %error,
                "failed to shut down a stalled terminal observer stream"
            );
        }
        io::Error::new(
            io::ErrorKind::TimedOut,
            "terminal observer stopped receiving output",
        )
    };
    let mut progress = now();
    while !data.is_empty() {
        match socket.write(data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => {
                data = &data[written..];
                progress = now();
                continue;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
        let remaining = remaining_until(progress + stall_timeout, now()).ok_or_else(timed_out)?;
        match poll_fd(socket.as_raw_fd(), libc::POLLOUT, remaining) {
            Ok(false) => return Err(timed_out()),
            Ok(true) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub fn wait_client_stream_readable(stream: &crate::ipc::LocalStream) -> std::io::Result<()> {
    use std::os::fd::AsFd as _;
    // Bound cancellation latency without polling idle connections hundreds of times per second.
    match poll_fd_readable(
        stream.as_fd().as_raw_fd(),
        super::limits::CLIENT_STREAM_POLL_INTERVAL,
    ) {
        Err(error) if error.kind() != std::io::ErrorKind::Interrupted => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_observer_write_times_out_when_the_injected_clock_passes_its_timeout() {
        let dir = shepr_test_support::ScratchDir::new("client-stream-stall");
        let path = dir.join("s.sock");
        let listener = crate::ipc::bind_local_listener(&path).expect("test precondition");
        let mut observer = crate::ipc::connect_local_stream(&path).expect("test precondition");
        let writer = listener.accept().expect("test precondition").0;
        // The observer never reads. Production uses the same nonblocking
        // writer path with an explicit stall timeout.
        // Twice the elapsed bound below, and inside the per-test budget, so a
        // writer that waits out any real stall fails the bound, not the budget.
        let timeout = Duration::from_secs(10);
        // Every read moves the injected clock a full timeout on, so the first
        // stall is already at its deadline.
        let started = Instant::now();
        let reads = std::cell::Cell::new(0_u32);
        let now = || {
            let read = reads.get();
            reads.set(read + 1);
            started + timeout * read
        };
        let payload = vec![b'x'; 16 * 1024 * 1024];
        let error = write_client_stream_with_clock(&writer, &payload, timeout, &now)
            .expect_err("a stalled observer cannot take the whole payload");
        let elapsed = started.elapsed();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        // Only a writer that ignored the injected clock waits out the timeout.
        assert!(
            elapsed < Duration::from_secs(5),
            "the write took {elapsed:?}"
        );
        // The stalled stream is shut down, so the observer drains to EOF
        // instead of blocking on a writer that gave up.
        observer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("test precondition");
        let mut drained = Vec::new();
        observer
            .read_to_end(&mut drained)
            .expect("the observer reaches EOF");
        assert!(drained.len() < payload.len());
    }
}
