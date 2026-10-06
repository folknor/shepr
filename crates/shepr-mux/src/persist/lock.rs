//! Exclusive ownership of a session state directory.

use std::path::Path;

/// Acquired before restore and held through the final save. Possession of this
/// value is required to construct a session writer.
pub struct DataDirLease {
    inner: shepr_platform::DataDirectoryLease,
}

impl DataDirLease {
    pub fn acquire(directory: &Path) -> Result<Self, shepr_platform::LeaseAcquireError> {
        Ok(Self {
            inner: shepr_platform::DataDirectoryLease::acquire(&shepr_paths::data_dir_lease_path(
                directory,
            ))?,
        })
    }

    pub fn directory(&self) -> &Path {
        self.inner.directory()
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
        let refusal = DataDirLease::acquire(&directory)
            .err()
            .expect("a held lease refuses a second owner");
        assert_eq!(refusal.kind(), std::io::ErrorKind::ResourceBusy);
        let shepr_platform::LeaseAcquireError::Held(held) = refusal else {
            panic!("expected held lease")
        };
        assert_eq!(held.directory(), lease.directory());
        drop(lease);
        DataDirLease::acquire(&directory).expect("lease after release");
    }

    #[test]
    fn lock_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = crate::test_support::ScratchDir::new("lease-private");
        let lease = DataDirLease::acquire(&scratch.join("data")).expect("lease");
        let lock_path = shepr_paths::data_dir_lease_path(lease.directory());
        let mode = std::fs::metadata(lock_path)
            .expect("lock file")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
