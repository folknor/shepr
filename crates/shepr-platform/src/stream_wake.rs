//! A cancellable wait for a local stream to become readable.

use std::os::fd::{AsFd as _, AsRawFd};

/// Wakes a thread blocked until a local stream is readable, from another
/// thread, without touching the stream itself.
pub struct StreamWake {
    reader: std::os::unix::net::UnixStream,
    writer: std::os::unix::net::UnixStream,
}

impl StreamWake {
    pub fn new() -> std::io::Result<Self> {
        let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
        Ok(Self { reader, writer })
    }

    pub fn cancel(&self) -> std::io::Result<()> {
        // EOF stays readable, including when cancellation precedes the wait.
        self.writer.shutdown(std::net::Shutdown::Write)
    }

    /// Blocks until `stream` is readable (or closed) or [`Self::cancel`] ran.
    pub fn wait(&self, stream: &crate::ipc::LocalStream) -> std::io::Result<()> {
        let mut descriptors = [
            libc::pollfd {
                fd: stream.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: both descriptors remain borrowed and the array has two entries.
            if unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) } >= 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}
