//! Detaching a daemon's stderr once its durable log is running.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd as _;

/// Points this process's stderr (fd 2) at `/dev/null`.
///
/// A client-spawned server starts with its stderr on a boot log that the
/// launcher bounds only while it waits for readiness. Once the server's file
/// log is running, calling this keeps a later stray write from growing that
/// file without limit, so it only ever holds pre-logging failures.
pub fn redirect_stderr_to_null() -> io::Result<()> {
    let null = OpenOptions::new().write(true).open("/dev/null")?;
    loop {
        // SAFETY: dup2(2) copies the open descriptor `null` over fd 2; it
        // touches no memory of this process, and `null` stays open across the
        // call. The previous fd 2 is closed by the kernel as part of it.
        let result = unsafe { libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO) };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
