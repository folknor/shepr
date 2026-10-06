use std::io;
use std::path::{Path, PathBuf};

use crate::ipc::{FlockLock, LockWait, acquire_flock_lock};

/// Exclusive ownership of a session data directory's lease file.
pub struct DataDirectoryLease {
    directory: PathBuf,
    _lock: FlockLock,
}

impl DataDirectoryLease {
    /// Creates and locks the lease at `path`, returning a busy error if
    /// another process already owns it.
    pub fn acquire(path: &Path) -> Result<Self, LeaseAcquireError> {
        let Some(file_name) = path.file_name() else {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "lease path must name a file").into(),
            );
        };
        let directory = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));

        crate::create_private_directory_all(directory)?;
        let directory = std::fs::canonicalize(directory)?;
        let canonical_path = directory.join(file_name);
        let Some(lock) = try_acquire(&canonical_path)? else {
            return Err(LeaseAcquireError::Held(DataDirectoryLeaseHeld {
                directory,
            }));
        };
        Ok(Self {
            directory,
            _lock: lock,
        })
    }

    /// Canonical data directory whose lease this value represents.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Briefly takes and releases the lease at `path`; `false` means held.
    pub fn probe(path: &Path) -> io::Result<bool> {
        Ok(try_acquire(path)?.is_some())
    }
}

fn try_acquire(path: &Path) -> io::Result<Option<FlockLock>> {
    match acquire_flock_lock(path, LockWait::FailIfHeld) {
        Ok(lock) => Ok(Some(lock)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

/// Ownership refusal is distinct from filesystem or lock failures.
#[derive(Debug)]
pub enum LeaseAcquireError {
    Held(DataDirectoryLeaseHeld),
    Io(io::Error),
}

impl LeaseAcquireError {
    pub fn kind(&self) -> io::ErrorKind {
        match self {
            Self::Held(_) => io::ErrorKind::ResourceBusy,
            Self::Io(error) => error.kind(),
        }
    }
}

impl From<io::Error> for LeaseAcquireError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for LeaseAcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for LeaseAcquireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Held(error) => Some(error),
            Self::Io(error) => Some(error),
        }
    }
}

/// Another process already holds the lease on a session data directory.
#[derive(Debug)]
pub struct DataDirectoryLeaseHeld {
    directory: PathBuf,
}

impl DataDirectoryLeaseHeld {
    /// The canonical session data directory another process holds.
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

impl std::fmt::Display for DataDirectoryLeaseHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "another shepr server owns the session files in {}",
            self.directory.display()
        )
    }
}

impl std::error::Error for DataDirectoryLeaseHeld {}
