//! Exclusive ownership of a session state directory.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The name is owned by `shepr-config`, where a stop (which does not link this
/// crate) finds the same file to wait for its release.
pub(super) const LOCK_FILE_NAME: &str = shepr_config::DATA_DIR_LEASE_FILE_NAME;

/// Acquired before restore and held through the final save. Possession of this
/// value is required to construct a session writer.
pub struct DataDirLease {
    directory: PathBuf,
    file: Option<File>,
}

impl DataDirLease {
    pub fn acquire(directory: &Path) -> io::Result<Self> {
        shepr_platform::create_private_directory_all(directory)?;
        let directory = std::fs::canonicalize(directory)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(directory.join(LOCK_FILE_NAME))?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                directory,
                file: Some(file),
            }),
            Err(TryLockError::WouldBlock) => Err(DataDirLeaseHeld::error(directory)),
            Err(TryLockError::Error(err)) => Err(err),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub(super) fn is_active(&self) -> bool {
        self.file.is_some()
    }

    pub fn release(&mut self) {
        self.file.take();
    }
}

/// Another process already holds the lease on a session data directory.
///
/// [`DataDirLease::acquire`] reports it as an [`io::ErrorKind::ResourceBusy`]
/// error carrying this payload, so the directory survives whichever caller
/// sees it. Callers that word the refusal themselves find it with
/// [`DataDirLeaseHeld::from_io`].
#[derive(Debug)]
pub struct DataDirLeaseHeld {
    directory: PathBuf,
}

impl DataDirLeaseHeld {
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

impl std::fmt::Display for DataDirLeaseHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "another shepr server owns the session files in {}",
            self.directory.display()
        )
    }
}

impl std::error::Error for DataDirLeaseHeld {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_excludes_other_owners_and_releases_on_drop() {
        let scratch = crate::test_support::ScratchDir::new("lease");
        let directory = scratch.join("data");
        let lease = DataDirLease::acquire(&directory).expect("first lease");
        let refusal = DataDirLease::acquire(&directory)
            .err()
            .expect("a held lease refuses a second owner");
        assert_eq!(refusal.kind(), io::ErrorKind::ResourceBusy);
        let held = DataDirLeaseHeld::from_io(&refusal).expect("the refusal names the directory");
        assert_eq!(held.directory(), lease.directory());
        drop(lease);
        DataDirLease::acquire(&directory).expect("lease after release");
    }

    #[test]
    fn lock_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = crate::test_support::ScratchDir::new("lease-private");
        let lease = DataDirLease::acquire(&scratch.join("data")).expect("lease");
        let mode = std::fs::metadata(lease.directory().join(LOCK_FILE_NAME))
            .expect("lock file")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
