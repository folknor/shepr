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

pub struct PublishOptions<'a> {
    pub preserve_metadata_from: Option<&'a Path>,
    /// Refuse symlinks and every other non-regular existing target.
    /// This inspects the final component; rename never follows that component.
    pub refuse_symlink_target: bool,
    pub durability: Durability,
    pub replace: bool,
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
    replace: bool,
}

impl PreparedFile {
    pub fn prepare(
        target: &Path,
        source: &mut impl io::Read,
        options: &PublishOptions<'_>,
    ) -> io::Result<Self> {
        let parent = parent(target);
        for _ in 0..crate::limits::RANDOM_NAME_ATTEMPTS {
            let token = crate::unpredictable_token()?;
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let temporary = publication_temporary_path(parent, ".shepr", token, sequence);
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
        if options.replace && matches!(options.durability, Durability::DirectoryOrWithdraw) {
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
            replace: options.replace,
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
            Ok(_) if !self.replace => {
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
        if self.replace {
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

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}
fn cleanup(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), %error, "failed to remove publication artifact");
    }
}

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

pub fn publish_file(
    target: &Path,
    source: &mut impl io::Read,
    options: &PublishOptions<'_>,
) -> io::Result<Published> {
    PreparedFile::prepare(target, source, options)?.commit()
}
