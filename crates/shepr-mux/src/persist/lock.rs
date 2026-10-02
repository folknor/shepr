//! Exclusive ownership of a session state directory.

use std::io;
use std::path::Path;

/// Config owns the lease filename used to build API paths; platform is below
/// config in the crate layers and owns opening and locking that path.
pub(super) const LOCK_FILE_NAME: &str = shepr_config::DATA_DIR_LEASE_FILE_NAME;

/// Acquired before restore and held through the final save. Possession of this
/// value is required to construct a session writer.
pub struct DataDirLease {
    inner: shepr_platform::DataDirectoryLease,
}

impl DataDirLease {
    pub fn acquire(directory: &Path) -> io::Result<Self> {
        Ok(Self {
            inner: shepr_platform::DataDirectoryLease::acquire(directory, LOCK_FILE_NAME)?,
        })
    }

    pub fn directory(&self) -> &Path {
        self.inner.directory()
    }

    pub(super) fn is_active(&self) -> bool {
        self.inner.is_active()
    }

    pub fn release(&mut self) {
        self.inner.release();
    }
}

pub use shepr_platform::DataDirectoryLeaseHeld as DataDirLeaseHeld;

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
