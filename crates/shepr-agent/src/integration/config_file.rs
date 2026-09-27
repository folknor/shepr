//! Protected writes for user-owned integration configuration, not managed assets.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
mod tests;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Holds the persistent sidecar lock for one user-owned config file.
pub(super) struct ConfigUpdateLock {
    /// `None` when the config's directory does not exist: there is nothing to
    /// edit, and creating the directory only for a lock would leave debris.
    _file: Option<std::fs::File>,
}

/// Serializes Shepr's read-modify-write of a user config across processes.
/// Callers hold the returned guard from before reading the config through its
/// atomic replacement to prevent concurrent edits from overwriting one another.
pub(super) fn lock_config_for_update(path: &Path) -> io::Result<ConfigUpdateLock> {
    check_config_target(path)?;
    let target = resolve_target(path)?;
    if target.parent().is_some_and(|parent| !parent.is_dir()) {
        return Ok(ConfigUpdateLock { _file: None });
    }
    let mut lock_name = target.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&lock_path)?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid takes no pointers and only returns the caller's effective user id.
    let effective_uid = unsafe { libc::geteuid() };
    if !metadata.is_file() || metadata.uid() != effective_uid {
        return Err(io::Error::other(
            "integration config lock must be a regular file owned by this user",
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;

    loop {
        // SAFETY: flock uses only the open descriptor owned by file.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result == 0 {
            return Ok(ConfigUpdateLock { _file: Some(file) });
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
}

/// Check before changing assets as well as immediately before replacing a config.
/// This is deliberately not config parsing or a transaction across multiple files.
pub(super) fn check_config_targets(dir: &Path, names: &[&str]) -> io::Result<()> {
    for name in names {
        check_config_target(&dir.join(name))?;
    }
    Ok(())
}

pub(super) fn check_config_target(path: &Path) -> io::Result<()> {
    reject_hard_links(path)?;
    resolve_target(path).map(|_| ())
}

fn reject_hard_links(path: &Path) -> io::Result<()> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_file() && shepr_platform::config_file_link_count(path)? > 1 {
        return Err(io::Error::other(format!(
            "cannot update {}: config has multiple hard links; use a separate file or a symlink before retrying",
            path.display()
        )));
    }
    Ok(())
}

// Unlike canonicalize, this also follows dangling symlinks on a first install.
fn resolve_target(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = fs::read_link(&current)?;
                current = if link.is_absolute() {
                    link
                } else {
                    current.parent().unwrap_or(Path::new(".")).join(link)
                };
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(io::Error::other(format!(
                    "cannot update {}: config is not a regular file",
                    path.display()
                )));
            }
            Ok(_) => return Ok(current),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::other(format!(
        "cannot update {}: too many symbolic links",
        path.display()
    )))
}

pub(super) fn write_config(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    check_config_target(path)?;
    let target = resolve_target(path)?;
    let replacement = Replacement::prepare(&target, contents.as_ref())?;
    replacement.commit()
}

struct Replacement {
    target: PathBuf,
    temporary: PathBuf,
}

impl Replacement {
    fn prepare(path: &Path, contents: &[u8]) -> io::Result<Self> {
        reject_hard_links(path)?;
        let target = resolve_target(path)?;
        let existing = match fs::metadata(&target) {
            Ok(_) => {
                // A writable parent must not let rename bypass file write permissions.
                OpenOptions::new().read(true).write(true).open(&target)?;
                Some(target.as_path())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let parent = target
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        for _ in 0..128 {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let temporary = parent.join(format!(
                ".shepr-config-{}-{sequence}.tmp",
                std::process::id()
            ));
            // Existing configs can contain secrets. Start their staging file private;
            // the platform writer preserves the original permissions before publication.
            // New configs retain ordinary create/umask/inherited-ACL defaults.
            let created = if existing.is_some() {
                shepr_platform::create_private_file(&temporary)
            } else {
                shepr_platform::create_config_temporary(&temporary)
            };
            match created {
                Ok(file) => drop(file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
            let replacement = Self {
                target: target.clone(),
                temporary,
            };
            shepr_platform::write_config_temporary(existing, &replacement.temporary, contents)?;
            return Ok(replacement);
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique config temporary file",
        ))
    }

    fn commit(self) -> io::Result<()> {
        reject_hard_links(&self.target)?;
        fs::rename(&self.temporary, &self.target)
    }
}

impl Drop for Replacement {
    fn drop(&mut self) {
        // After publication the temporary name is absent. Never remove the target.
        if let Err(error) = fs::remove_file(&self.temporary)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.temporary.display(), %error, "failed to remove integration config temporary file");
        }
    }
}
