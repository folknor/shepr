use super::*;
use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    time::Instant,
};

fn shutdown_client_stream(stream: &interprocess::local_socket::Stream) -> std::io::Result<()> {
    let interprocess::local_socket::Stream::UdSocket(stream) = stream;
    stream.inner().shutdown(std::net::Shutdown::Both)
}

pub struct ClientStreamReader<'a>(pub &'a mut interprocess::local_socket::Stream);

impl Read for ClientStreamReader<'_> {
    fn read(&mut self, data: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.0.read(data) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let interprocess::local_socket::Stream::UdSocket(stream) = &*self.0;
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

pub fn write_client_stream(
    stream: &interprocess::local_socket::Stream,
    data: &[u8],
) -> std::io::Result<()> {
    // clock-io-ok: the public entry point supplies the real clock.
    write_client_stream_with_clock(stream, data, &Instant::now)
}

fn write_client_stream_with_clock(
    stream: &interprocess::local_socket::Stream,
    mut data: &[u8],
    now: &dyn Fn() -> Instant,
) -> std::io::Result<()> {
    use std::io;

    let interprocess::local_socket::Stream::UdSocket(socket) = stream;
    let mut socket = socket.inner();
    let Some(timeout) = socket.write_timeout()? else {
        return socket.write_all(data);
    };
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
        let wait_ms = poll_timeout_until(progress + timeout, now()).ok_or_else(timed_out)?;
        match poll_fd(socket.as_raw_fd(), libc::POLLOUT, wait_ms) {
            Ok(false) => return Err(timed_out()),
            Ok(true) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub fn wait_client_stream_readable(
    stream: &interprocess::local_socket::Stream,
) -> std::io::Result<()> {
    use std::os::fd::AsFd as _;
    let interprocess::local_socket::Stream::UdSocket(stream) = stream;
    // Bound cancellation latency without polling idle connections hundreds of times per second.
    match poll_fd_readable(
        stream.as_fd().as_raw_fd(),
        super::limits::CLIENT_STREAM_POLL_INTERVAL_MS,
    ) {
        Err(error) if error.kind() != std::io::ErrorKind::Interrupted => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::traits::Listener as _;
    use std::time::Duration;

    #[test]
    fn stalled_observer_write_times_out_when_the_injected_clock_passes_its_timeout() {
        let dir = shepr_test_support::ScratchDir::new("client-stream-stall");
        let path = dir.join("s.sock");
        let listener = crate::ipc::bind_local_listener(&path).expect("test precondition");
        let mut observer = crate::ipc::connect_local_stream(&path).expect("test precondition");
        let writer = listener.accept().expect("test precondition");
        let interprocess::local_socket::Stream::UdSocket(socket) = &writer;
        // Like the client writer: nonblocking, with the stall timeout as the
        // socket's write timeout. The observer never reads.
        // Twice the elapsed bound below, and inside the per-test budget, so a
        // writer that waits out any real stall fails the bound, not the budget.
        let timeout = Duration::from_secs(10);
        socket
            .inner()
            .set_nonblocking(true)
            .expect("test precondition");
        socket
            .inner()
            .set_write_timeout(Some(timeout))
            .expect("test precondition");

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
        let error = write_client_stream_with_clock(&writer, &payload, &now)
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
        let interprocess::local_socket::Stream::UdSocket(observer_socket) = &observer;
        observer_socket
            .inner()
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("test precondition");
        let mut drained = Vec::new();
        observer
            .read_to_end(&mut drained)
            .expect("the observer reaches EOF");
        assert!(drained.len() < payload.len());
    }
}
