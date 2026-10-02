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

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The longest socket path, in bytes, that Linux accepts: `sun_path` less its
/// terminating NUL.
///
/// `the_limit_is_what_std_accepts_for_a_socket_address` checks this against
/// the standard library's own `sockaddr_un` construction, so the number is
/// proven rather than restated.
// limits-exempt: this is the Linux sockaddr_un ABI limit, kept beside the path check and its proof.
pub const UNIX_SOCKET_PATH_MAX: usize = 107;

/// A pathname that can be represented by Linux `sockaddr_un::sun_path`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SocketPath(PathBuf);

impl SocketPath {
    /// Validate and own a pathname suitable for a Unix domain socket.
    pub fn new(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        validate_socket_path(&path)?;
        Ok(Self(path))
    }

    /// Borrow the validated pathname.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consume this value and return its pathname.
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for SocketPath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

fn validate_socket_path(path: &Path) -> io::Result<()> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > UNIX_SOCKET_PATH_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Unix socket path is {} bytes; the Linux limit is {UNIX_SOCKET_PATH_MAX}: {}",
                bytes.len(),
                path.display()
            ),
        ));
    }
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unix socket path cannot be empty",
        ));
    }
    if bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Unix socket path contains a NUL byte: {}", path.display()),
        ));
    }
    Ok(())
}

/// Whether `path` can name a Unix socket within Linux's pathname constraints.
#[must_use]
pub fn fits_unix_socket_path(path: &Path) -> bool {
    validate_socket_path(path).is_ok()
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
