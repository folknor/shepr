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
    mut data: &[u8],
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
                err = %error,
                "failed to shut down a stalled terminal observer stream"
            );
        }
        io::Error::new(
            io::ErrorKind::TimedOut,
            "terminal observer stopped receiving output",
        )
    };
    let mut progress = Instant::now();
    while !data.is_empty() {
        match socket.write(data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => {
                data = &data[written..];
                progress = Instant::now();
                continue;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
        let wait_ms = poll_timeout_until(progress + timeout).ok_or_else(timed_out)?;
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
    match poll_fd_readable(stream.as_fd().as_raw_fd(), 100) {
        Err(error) if error.kind() != std::io::ErrorKind::Interrupted => Err(error),
        _ => Ok(()),
    }
}
