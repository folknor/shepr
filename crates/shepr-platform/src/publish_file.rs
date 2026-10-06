//! Atomic publication with explicit permission and crash-durability policies.
use std::fs;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
pub enum Durability {
    /// Sync the contents, but do not sync the containing directory.
    FileOnly,
    /// Keep a published replacement and report uncertain crash durability.
    Directory,
    /// Withdraw a newly published, exclusively named file if directory sync fails.
    DirectoryOrWithdraw,
}

/// What publication does with a target that already exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishTarget {
    /// Overwrite it atomically by rename.
    ReplaceExisting,
    /// Refuse it with `AlreadyExists`, linking the new file in no-clobber.
    CreateOnly,
}

pub struct PublishOptions<'a> {
    pub preserve_metadata_from: Option<&'a Path>,
    /// Refuse symlinks and every other non-regular existing target.
    /// This inspects the final component; rename never follows that component.
    pub refuse_symlink_target: bool,
    pub durability: Durability,
    pub existing: PublishTarget,
    /// Creation mode, filtered by umask; preserved metadata takes precedence.
    pub mode: u32,
}

#[derive(Debug)]
pub enum Published {
    /// Every sync requested by the selected durability policy succeeded.
    Durable,
    NotDurable(io::Error),
}

/// A fully written, synced file awaiting atomic publication. Dropping it
/// removes its staging name. Callers may perform their own conflict check
/// before commit without exposing partially written contents.
pub struct PreparedFile {
    target: PathBuf,
    temporary: PathBuf,
    output: fs::File,
    refuse_symlink_target: bool,
    durability: Durability,
    existing: PublishTarget,
}

impl PreparedFile {
    pub fn prepare(
        target: &Path,
        source: &mut impl io::Read,
        options: &PublishOptions<'_>,
    ) -> io::Result<Self> {
        let parent = parent(target);
        Self::prepare_named(target, source, options, || {
            let token = crate::unpredictable_token()?;
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            Ok(publication_temporary_path(
                parent,
                STAGING_PREFIX,
                token,
                sequence,
            ))
        })
    }

    /// `prepare` over a source of staging names; a name already taken is
    /// neither used nor removed, and the next one is tried.
    fn prepare_named(
        target: &Path,
        source: &mut impl io::Read,
        options: &PublishOptions<'_>,
        mut next_name: impl FnMut() -> io::Result<PathBuf>,
    ) -> io::Result<Self> {
        for _ in 0..crate::limits::RANDOM_NAME_ATTEMPTS {
            let temporary = next_name()?;
            match Self::prepare_at(target, &temporary, source, options) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                result => return result,
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate publication temporary",
        ))
    }

    /// Use an exclusively created caller-selected staging name. A caller
    /// reclaiming that name must first exclude every other writer.
    pub fn prepare_at(
        target: &Path,
        temporary: &Path,
        source: &mut impl io::Read,
        options: &PublishOptions<'_>,
    ) -> io::Result<Self> {
        if options.existing == PublishTarget::ReplaceExisting
            && matches!(options.durability, Durability::DirectoryOrWithdraw)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "withdrawal requires exclusive publication",
            ));
        }
        let output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if options.preserve_metadata_from.is_some() {
                0o600
            } else {
                options.mode
            })
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(temporary)?;
        let mut prepared = Self {
            target: target.to_owned(),
            temporary: temporary.to_owned(),
            output,
            refuse_symlink_target: options.refuse_symlink_target,
            durability: options.durability,
            existing: options.existing,
        };
        if options.preserve_metadata_from.is_some() {
            crate::config_file::preserve_metadata(
                options.preserve_metadata_from,
                &prepared.output,
            )?;
        }
        io::copy(source, &mut prepared.output)?;
        prepared.output.sync_all()?;
        Ok(prepared)
    }

    pub fn temporary_path(&self) -> &Path {
        &self.temporary
    }

    /// Set managed-asset permissions through the open inode, then sync them.
    pub fn set_mode(&self, mode: u32) -> io::Result<()> {
        self.output
            .set_permissions(fs::Permissions::from_mode(mode))?;
        self.output.sync_all()
    }

    pub fn commit(self) -> io::Result<Published> {
        self.commit_with_directory_sync(crate::sync_directory)
    }

    pub fn commit_with_directory_sync(
        self,
        sync: impl FnOnce(&Path) -> io::Result<()>,
    ) -> io::Result<Published> {
        match fs::symlink_metadata(&self.target) {
            Ok(_) if self.existing == PublishTarget::CreateOnly => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "publish target already exists",
                ));
            }
            Ok(metadata) if self.refuse_symlink_target && !metadata.is_file() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to replace a non-file publication target",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if self.existing == PublishTarget::ReplaceExisting {
            fs::rename(&self.temporary, &self.target)?;
        } else {
            // Linking is an atomic no-clobber publication in the same directory.
            fs::hard_link(&self.temporary, &self.target)?;
            // Publication already succeeded. A failed staging-name cleanup
            // must not pretend that readers cannot see the new target.
            cleanup(&self.temporary);
        }
        if matches!(self.durability, Durability::FileOnly) {
            return Ok(Published::Durable);
        }
        match sync(parent(&self.target)) {
            Ok(()) => Ok(Published::Durable),
            Err(error) if matches!(self.durability, Durability::DirectoryOrWithdraw) => {
                cleanup(&self.target);
                Err(error)
            }
            Err(error) => Ok(Published::NotDurable(error)),
        }
    }
}

impl Drop for PreparedFile {
    fn drop(&mut self) {
        cleanup(&self.temporary);
    }
}

/// Prepares a private publication with the shared session and metadata-cache
/// policy: new files use `mode`, replacements use rename and keep a completed
/// publish if the directory sync fails, and exclusive creates are withdrawn
/// if that sync fails. The final target component must be absent or regular.
///
/// Use [`publish_private`] when no work is needed between staging and commit.
pub fn prepare_private(
    target: &Path,
    source: &mut impl io::Read,
    mode: u32,
    existing: PublishTarget,
) -> io::Result<PreparedFile> {
    let durability = match existing {
        PublishTarget::ReplaceExisting => Durability::Directory,
        PublishTarget::CreateOnly => Durability::DirectoryOrWithdraw,
    };
    PreparedFile::prepare(
        target,
        source,
        &PublishOptions {
            preserve_metadata_from: None,
            refuse_symlink_target: true,
            durability,
            existing,
            mode,
        },
    )
}

/// Atomically publishes private contents with the common file and directory
/// durability policy. The staging file is created with `mode`; an existing
/// target is handled according to `existing`.
pub fn publish_private(
    target: &Path,
    source: &mut impl io::Read,
    mode: u32,
    existing: PublishTarget,
) -> io::Result<Published> {
    publish_private_with_directory_sync(target, source, mode, existing, crate::sync_directory)
}

/// Atomically publishes private contents while using the caller's directory
/// sync operation. This keeps filesystem policy shared and leaves the sync
/// seam available to callers that need to observe or control that step.
pub fn publish_private_with_directory_sync(
    target: &Path,
    source: &mut impl io::Read,
    mode: u32,
    existing: PublishTarget,
    sync_directory: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<Published> {
    prepare_private(target, source, mode, existing)?.commit_with_directory_sync(sync_directory)
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}
fn cleanup(path: &Path) {
    // This low-level cleanup runs from PreparedFile::drop and reports only
    // the staging artifact. Session recovery cleanup has a separate
    // persist-specific event and remains with that caller.
    if let Err(error) = fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        crate::structured_log!(
            WARN, event = publish_file.cleanup, outcome = Error,
            path = %path.display(), %error, "failed to remove publication artifact"
        );
    }
}

/// The prefix of every staging name [`PreparedFile::prepare`] chooses.
const STAGING_PREFIX: &str = ".shepr";

/// A sibling staging name: a full-width random token separates processes,
/// a sequence separates calls, and exclusive creation arbitrates collisions.
pub fn publication_temporary_path(
    parent: &Path,
    prefix: &str,
    token: u64,
    sequence: u64,
) -> PathBuf {
    parent.join(format!("{prefix}-{token:016x}-{sequence}.tmp"))
}

/// Whether `name` has the shape of a staging name [`PreparedFile::prepare`]
/// chooses. A process that exits mid-publication leaves such a file behind;
/// only an owner that excludes every other writer of its directory may remove
/// one, since a live publication uses the same shape.
pub fn is_staging_name(name: &str) -> bool {
    let Some(body) = name
        .strip_prefix(STAGING_PREFIX)
        .and_then(|name| name.strip_prefix('-'))
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    let Some((token, sequence)) = body.split_once('-') else {
        return false;
    };
    // limits-exempt: the hex width of the u64 token in the staging name format.
    token.len() == 16
        && token.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !sequence.is_empty()
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(durability: Durability, existing: PublishTarget) -> PublishOptions<'static> {
        PublishOptions {
            preserve_metadata_from: None,
            refuse_symlink_target: true,
            durability,
            existing,
            mode: 0o600,
        }
    }

    fn prepare(target: &Path, contents: &[u8], options: &PublishOptions<'_>) -> PreparedFile {
        PreparedFile::prepare(target, &mut &contents[..], options).expect("prepare")
    }

    #[test]
    fn exclusive_publication_does_not_clobber_an_existing_target() {
        let scratch = shepr_test_support::ScratchDir::new("publish-no-clobber");
        let target = scratch.path().join("target");
        fs::write(&target, b"original").expect("write");
        let prepared = prepare(
            &target,
            b"new",
            &options(Durability::FileOnly, PublishTarget::CreateOnly),
        );
        let temporary = prepared.temporary_path().to_owned();
        let error = prepared.commit().expect_err("the target exists");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&target).expect("read"), b"original");
        assert!(
            !temporary.try_exists().expect("stat"),
            "dropping removes the staging name"
        );
    }

    #[test]
    fn exclusive_publication_links_the_file_and_drops_the_staging_name() {
        let scratch = shepr_test_support::ScratchDir::new("publish-link");
        let target = scratch.path().join("target");
        let prepared = prepare(
            &target,
            b"new",
            &options(Durability::FileOnly, PublishTarget::CreateOnly),
        );
        let temporary = prepared.temporary_path().to_owned();
        assert!(matches!(
            prepared.commit().expect("commit"),
            Published::Durable
        ));
        assert_eq!(fs::read(&target).expect("read"), b"new");
        assert!(!temporary.try_exists().expect("stat"));
    }

    #[test]
    fn withdraw_removes_the_published_file_when_the_directory_sync_fails() {
        let scratch = shepr_test_support::ScratchDir::new("publish-withdraw");
        let target = scratch.path().join("target");
        let prepared = prepare(
            &target,
            b"new",
            &options(Durability::DirectoryOrWithdraw, PublishTarget::CreateOnly),
        );
        let error = prepared
            .commit_with_directory_sync(|_| Err(io::Error::other("sync failed")))
            .expect_err("the sync failure is reported");
        assert_eq!(error.to_string(), "sync failed");
        assert!(
            !target.try_exists().expect("stat"),
            "the unsynced publication is withdrawn"
        );
    }

    #[test]
    fn a_failed_directory_sync_keeps_a_replacement_and_reports_it_not_durable() {
        let scratch = shepr_test_support::ScratchDir::new("publish-not-durable");
        let target = scratch.path().join("target");
        fs::write(&target, b"original").expect("write");
        let prepared = prepare(
            &target,
            b"new",
            &options(Durability::Directory, PublishTarget::ReplaceExisting),
        );
        let published = prepared
            .commit_with_directory_sync(|_| Err(io::Error::other("sync failed")))
            .expect("the replacement stays published");
        assert!(matches!(published, Published::NotDurable(_)));
        assert_eq!(fs::read(&target).expect("read"), b"new");
    }

    #[test]
    fn a_taken_staging_name_is_neither_used_nor_removed() {
        let scratch = shepr_test_support::ScratchDir::new("publish-collision");
        let target = scratch.path().join("target");
        let taken = scratch.path().join("taken.tmp");
        let free = scratch.path().join("free.tmp");
        fs::write(&taken, b"unrelated file").expect("write");
        let mut names = vec![free.clone(), taken.clone()];
        let prepared = PreparedFile::prepare_named(
            &target,
            &mut &b"new"[..],
            &options(Durability::FileOnly, PublishTarget::ReplaceExisting),
            || Ok(names.pop().expect("a name")),
        )
        .expect("the second name is free");
        assert_eq!(prepared.temporary_path(), free);
        assert_eq!(fs::read(&taken).expect("read"), b"unrelated file");
        drop(prepared);
        assert_eq!(fs::read(&taken).expect("read"), b"unrelated file");
        assert!(
            !free.try_exists().expect("stat"),
            "dropping removes only its own staging name"
        );
    }

    #[test]
    fn withdrawal_requires_exclusive_publication() {
        let scratch = shepr_test_support::ScratchDir::new("publish-withdraw-replace");
        let target = scratch.path().join("target");
        let Err(error) = PreparedFile::prepare(
            &target,
            &mut &b"x"[..],
            &options(
                Durability::DirectoryOrWithdraw,
                PublishTarget::ReplaceExisting,
            ),
        ) else {
            panic!("withdrawal and replacement conflict");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn staging_names_are_recognized_by_their_shape_only() {
        let named = publication_temporary_path(Path::new("/d"), STAGING_PREFIX, u64::MAX, 7);
        let name = named
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a UTF-8 file name");
        assert!(is_staging_name(name));
        let small = publication_temporary_path(Path::new("/d"), STAGING_PREFIX, 1, 0);
        assert!(is_staging_name(
            small
                .file_name()
                .and_then(|name| name.to_str())
                .expect("name")
        ));
        for other in [
            "session.json",
            ".shepr-0123456789abcdef-1.json",
            ".shepr-0123456789abcde-1.tmp",
            ".shepr-0123456789abcdeg-1.tmp",
            ".shepr-0123456789abcdef-.tmp",
            ".other-0123456789abcdef-1.tmp",
        ] {
            assert!(!is_staging_name(other), "{other}");
        }
    }

    #[test]
    fn a_symlink_target_is_refused_and_left_alone() {
        let scratch = shepr_test_support::ScratchDir::new("publish-symlink");
        let real = scratch.path().join("real");
        fs::write(&real, b"original").expect("write");
        let target = scratch.path().join("target");
        std::os::unix::fs::symlink(&real, &target).expect("symlink");
        let prepared = prepare(
            &target,
            b"new",
            &options(Durability::FileOnly, PublishTarget::ReplaceExisting),
        );
        let error = prepared.commit().expect_err("a symlink is not replaced");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(&real).expect("read"), b"original");
        assert!(
            fs::symlink_metadata(&target)
                .expect("metadata")
                .file_type()
                .is_symlink()
        );
    }
}
