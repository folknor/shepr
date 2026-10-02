use std::io;
use std::path::{Path, PathBuf};

use crate::ipc::{FlockLock, acquire_flock_lock};

/// Exclusive ownership of a session data directory's lease file.
pub struct DataDirectoryLease {
    directory: PathBuf,
    lock: Option<FlockLock>,
}

impl DataDirectoryLease {
    /// Creates and locks `file_name` in `directory`, returning a busy error if
    /// another process already owns it.
    pub fn acquire(directory: &Path, file_name: &str) -> io::Result<Self> {
        if file_name.is_empty() || file_name == "." || file_name == ".." || file_name.contains('/')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "lease file name must be a single path component",
            ));
        }

        crate::create_private_directory_all(directory)?;
        let directory = std::fs::canonicalize(directory)?;
        let path = directory.join(file_name);
        let Some(lock) = try_acquire(&path)? else {
            return Err(DataDirectoryLeaseHeld::error(directory));
        };
        Ok(Self {
            directory,
            lock: Some(lock),
        })
    }

    /// Canonical data directory whose lease this value represents.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Whether this value still owns its lock.
    pub fn is_active(&self) -> bool {
        self.lock.is_some()
    }

    /// Releases ownership while retaining the canonical directory path.
    pub fn release(&mut self) {
        self.lock.take();
    }

    /// Briefly takes and releases the lease at `path`; `false` means held.
    pub fn probe(path: &Path) -> io::Result<bool> {
        Ok(try_acquire(path)?.is_some())
    }
}

fn try_acquire(path: &Path) -> io::Result<Option<FlockLock>> {
    match acquire_flock_lock(path, false) {
        Ok(lock) => Ok(Some(lock)),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

/// Another process already holds the lease on a session data directory.
#[derive(Debug)]
pub struct DataDirectoryLeaseHeld {
    directory: PathBuf,
}

impl DataDirectoryLeaseHeld {
    fn error(directory: PathBuf) -> io::Error {
        io::Error::new(io::ErrorKind::ResourceBusy, Self { directory })
    }

    /// The held-lease refusal inside `error`, if it is one.
    pub fn from_io(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref::<Self>()
    }

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
