use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::limits::TEMP_FILE_ALLOCATION_ATTEMPTS;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

fn temporary_path(parent: &Path, prefix: &str, sequence: u64) -> io::Result<PathBuf> {
    let token = temporary_token()?;

    Ok(shepr_platform::publish_file::publication_temporary_path(
        parent, prefix, token, sequence,
    ))
}

#[cfg(not(test))]
fn temporary_token() -> io::Result<u64> {
    shepr_platform::unpredictable_token()
}

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

pub(super) struct AtomicReplace {
    target: PathBuf,
    prepared: shepr_platform::publish_file::PreparedFile,
}

impl AtomicReplace {
    pub(super) fn prepare_with_policy(
        target: &Path,
        temporary_prefix: &str,
        policy: PermissionPolicy<'_>,
        contents: &[u8],
    ) -> io::Result<Self> {
        let parent = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));

        for _ in 0..TEMP_FILE_ALLOCATION_ATTEMPTS {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            // The kernel-random token separates processes; the atomic sequence
            // separates threads, and create_new arbitrates collisions.
            let temporary = temporary_path(parent, temporary_prefix, sequence)?;
            use shepr_platform::publish_file::{Durability, PreparedFile, PublishOptions};
            let (existing, mode) = match policy {
                PermissionPolicy::ManagedAsset { .. } => (None, 0o666),
                PermissionPolicy::UserConfig { existing } => (existing, 0o666),
            };
            let prepared = match PreparedFile::prepare_at(
                target,
                &temporary,
                &mut &contents[..],
                &PublishOptions {
                    preserve_metadata_from: existing,
                    refuse_symlink_target: false,
                    durability: Durability::FileOnly,
                    replace: true,
                    mode,
                },
            ) {
                Ok(prepared) => prepared,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            if matches!(policy, PermissionPolicy::ManagedAsset { executable: true }) {
                prepared.set_mode(0o755)?;
            }
            return Ok(Self {
                target: target.to_path_buf(),
                prepared,
            });
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "could not allocate a unique temporary file next to {}",
                target.display()
            ),
        ))
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
use std::sync::OnceLock;

#[cfg(test)]
static TEST_TEMP_TOKEN: OnceLock<u64> = OnceLock::new();

/// The production token unless a test pinned one with `set_temp_token_for_test`.
#[cfg(test)]
fn temporary_token() -> io::Result<u64> {
    if let Some(token) = TEST_TEMP_TOKEN.get() {
        return Ok(*token);
    }

    shepr_platform::unpredictable_token()
}

#[cfg(test)]
pub(super) fn reset_temp_sequence(sequence: u64) {
    NEXT_TEMP.store(sequence, Ordering::Relaxed);
}

#[cfg(test)]
pub(super) fn set_temp_token_for_test(token: u64) {
    let stored = *TEST_TEMP_TOKEN.get_or_init(|| token);
    assert_eq!(stored, token, "the test temp token is set once per process");
}

#[cfg(test)]
pub(super) fn temporary_path_for_test(
    parent: &Path,
    prefix: &str,
    sequence: u64,
) -> io::Result<PathBuf> {
    temporary_path(parent, prefix, sequence)
}

#[cfg(test)]
impl AtomicReplace {
    pub(super) fn temporary_path(&self) -> &Path {
        self.prepared.temporary_path()
    }
}
