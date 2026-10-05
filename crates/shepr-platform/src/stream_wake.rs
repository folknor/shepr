//! Cancellable readiness waits for a local stream.

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
        self.wait_for(stream, libc::POLLIN)
    }

    /// Blocks until `stream` is writable (or closed) or cancellation ran.
    pub fn wait_writable(&self, stream: &crate::ipc::LocalStream) -> std::io::Result<()> {
        self.wait_for(stream, libc::POLLOUT)
    }

    fn wait_for(
        &self,
        stream: &crate::ipc::LocalStream,
        events: libc::c_short,
    ) -> std::io::Result<()> {
        let mut descriptors = [
            libc::pollfd {
                fd: stream.as_fd().as_raw_fd(),
                events,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::Arc;

    #[test]
    fn cancelled_writable_wait_releases_a_stalled_stream() {
        let (mut writer, _reader) = crate::ipc::LocalStream::pair().expect("socket pair");
        writer.set_nonblocking(true).expect("nonblocking writer");
        let bytes = [0_u8; 8192];
        loop {
            match writer.write(&bytes) {
                Ok(count) => assert!(count > 0),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("fill socket: {error}"),
            }
        }
        let wake = Arc::new(StreamWake::new().expect("cancellation socket"));
        let worker_wake = Arc::clone(&wake);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            done_tx
                .send(worker_wake.wait_writable(&writer))
                .expect("report wait");
        });
        // Cancellation also works when it races before poll starts.
        wake.cancel().expect("cancel writable wait");
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("wait was cancelled")
            .expect("poll succeeded");
        worker.join().expect("waiter ended");
    }
}
