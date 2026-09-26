//! Exclusive ownership of a session state directory.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub(super) const LOCK_FILE_NAME: &str = "session.lock";

/// Acquired before restore and held through the final save. Possession of this
/// value is required to construct a session writer.
pub(crate) struct DataDirLease {
    directory: PathBuf,
    file: Option<File>,
}

impl DataDirLease {
    pub(crate) fn acquire(directory: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(directory)?;
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
            Err(TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!(
                    "another shepr server owns the session files in {}",
                    directory.display()
                ),
            )),
            Err(TryLockError::Error(err)) => Err(err),
        }
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn release(&mut self) {
        self.file.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_excludes_other_owners_and_releases_on_drop() {
        let scratch = crate::test_support::ScratchDir::new("lease");
        let directory = scratch.join("data");
        let lease = DataDirLease::acquire(&directory).expect("first lease");
        assert_eq!(
            DataDirLease::acquire(&directory)
                .err()
                .map(|err| err.kind()),
            Some(io::ErrorKind::ResourceBusy)
        );
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
