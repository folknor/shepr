use std::io;
use std::path::Path;

/// How the staged file gets its permissions: managed assets are created
/// fresh, while a user config keeps the permissions of the file it replaces.
#[expect(
    variant_size_differences,
    reason = "a short-lived Copy argument, never stored in bulk, so the gap between the bool and the path reference costs nothing"
)]
#[derive(Clone, Copy)]
pub(super) enum PermissionPolicy<'a> {
    ManagedAsset { executable: bool },
    UserConfig { existing: Option<&'a Path> },
}

/// A staged replacement, named and collision-checked by the platform's
/// publication path.
pub(super) struct AtomicReplace {
    target: std::path::PathBuf,
    prepared: shepr_platform::publish_file::PreparedFile,
}

impl AtomicReplace {
    pub(super) fn prepare_with_policy(
        target: &Path,
        policy: PermissionPolicy<'_>,
        contents: &[u8],
    ) -> io::Result<Self> {
        use shepr_platform::publish_file::{Durability, PreparedFile, PublishOptions};
        let (existing, mode) = match policy {
            PermissionPolicy::ManagedAsset { .. } => (None, 0o666),
            PermissionPolicy::UserConfig { existing } => (existing, 0o666),
        };
        let prepared = PreparedFile::prepare(
            target,
            &mut &contents[..],
            &PublishOptions {
                preserve_metadata_from: existing,
                refuse_symlink_target: false,
                durability: Durability::FileOnly,
                existing: shepr_platform::publish_file::PublishTarget::ReplaceExisting,
                mode,
            },
        )?;
        if matches!(policy, PermissionPolicy::ManagedAsset { executable: true }) {
            prepared.set_mode(0o755)?;
        }
        Ok(Self {
            target: target.to_path_buf(),
            prepared,
        })
    }

    pub(super) fn commit(self) -> io::Result<()> {
        self.prepared.commit().map(|_| ())
    }

    pub(super) fn commit_after(
        self,
        before_publish: impl FnOnce(&Path) -> io::Result<()>,
    ) -> io::Result<()> {
        before_publish(&self.target)?;
        self.prepared.commit().map(|_| ())
    }
}

#[cfg(test)]
impl AtomicReplace {
    pub(super) fn temporary_path(&self) -> &Path {
        self.prepared.temporary_path()
    }
}
