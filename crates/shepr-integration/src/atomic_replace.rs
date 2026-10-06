use crate::types::{InstallError, InstallResult};
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

/// A staged replacement using the platform's shared `PreparedFile` path for
/// naming, collision checks, writing and atomic publication. Keep the generic
/// preparation here: `prepare_private` does not preserve existing metadata,
/// refuses a non-regular target and requests directory durability. Managed
/// assets intentionally replace a symlink, while integration writes use
/// file-only durability. User configs also preserve existing metadata and
/// defer commit until their lock and snapshot conflict checks have passed.
///
/// Process exit can leave a staged file behind because destructors do not run.
/// Do not sweep sibling staging files here: release processes with different
/// XDG state roots can hold different data-directory leases while sharing agent
/// directories, and managed-asset writes have no common lock. A sweep could
/// remove another live installer's file. Reclamation needs
/// an installer-wide ownership lock shared by managed assets and user configs.
pub(super) struct AtomicReplace {
    target: std::path::PathBuf,
    prepared: shepr_platform::publish_file::PreparedFile,
}

impl AtomicReplace {
    pub(super) fn prepare_with_policy(
        target: &Path,
        policy: PermissionPolicy<'_>,
        contents: &[u8],
    ) -> InstallResult<Self> {
        use shepr_platform::publish_file::{Durability, PreparedFile, PublishOptions};
        let existing = match policy {
            PermissionPolicy::ManagedAsset { .. } => None,
            PermissionPolicy::UserConfig { existing } => existing,
        };
        let prepared = PreparedFile::prepare(
            target,
            &mut &contents[..],
            &PublishOptions {
                preserve_metadata_from: existing,
                refuse_symlink_target: false,
                durability: Durability::FileOnly,
                existing: shepr_platform::publish_file::PublishTarget::ReplaceExisting,
                mode: 0o666,
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

    pub(super) fn commit(self) -> InstallResult<()> {
        self.prepared
            .commit()
            .map(|_| ())
            .map_err(InstallError::from)
    }

    pub(super) fn commit_after(
        self,
        before_publish: impl FnOnce(&Path) -> InstallResult<()>,
    ) -> InstallResult<()> {
        before_publish(&self.target)?;
        self.prepared
            .commit()
            .map(|_| ())
            .map_err(InstallError::from)
    }
}

#[cfg(test)]
impl AtomicReplace {
    pub(super) fn temporary_path(&self) -> &Path {
        self.prepared.temporary_path()
    }
}
