//! The Unix socket path limit, owned once.
//!
//! A Unix socket is bound and connected through `sockaddr_un`, whose
//! `sun_path` field is a fixed array that also holds the terminating NUL, so a
//! path longer than [`UNIX_SOCKET_PATH_MAX`] bytes cannot name a socket at all.
//! Every site that composes a socket path (the SSH bridge endpoints, the shared
//! OpenSSH control path and its staging name, the test scratch roots' socket
//! budget) asks here rather than restating the number.
//!
//! Core is the home because the test scratch fixture has to prove its socket
//! budget against the same limit production enforces, and it sits below the
//! platform crate.

use std::path::Path;

/// The longest socket path, in bytes, that Linux accepts: `sun_path` less its
/// terminating NUL.
///
/// `the_limit_is_what_std_accepts_for_a_socket_address` checks this against
/// the standard library's own `sockaddr_un` construction, so the number is
/// proven rather than restated.
pub const UNIX_SOCKET_PATH_MAX: usize = 107;

/// Whether `path` is short enough to name a Unix socket.
#[must_use]
pub fn fits_unix_socket_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().len() <= UNIX_SOCKET_PATH_MAX
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::SocketAddr;

    #[test]
    fn the_limit_is_what_std_accepts_for_a_socket_address() {
        let longest = "x".repeat(UNIX_SOCKET_PATH_MAX);
        let one_over = "x".repeat(UNIX_SOCKET_PATH_MAX + 1);
        assert!(SocketAddr::from_pathname(&longest).is_ok());
        assert!(SocketAddr::from_pathname(&one_over).is_err());
        assert!(fits_unix_socket_path(Path::new(&longest)));
        assert!(!fits_unix_socket_path(Path::new(&one_over)));
    }
}
