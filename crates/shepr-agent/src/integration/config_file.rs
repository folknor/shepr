//! Protected writes for user-owned integration configuration, not managed assets.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::limits::MAX_CONFIG_SYMLINK_DEPTH;

use super::atomic_replace::{AtomicReplace, PermissionPolicy};
use super::env::AgentIntegrationPaths;

/// Holds the persistent lock for one user-owned config file.
pub(super) struct ConfigUpdateLock {
    _lock: shepr_platform::ipc::FlockLock,
}

/// Serializes Shepr's read-modify-write of a user config across processes.
/// Callers hold the returned guard from before reading the config through its
/// atomic replacement to prevent concurrent edits from overwriting one another.
pub(super) fn lock_config_for_update(
    path: &Path,
    paths: &AgentIntegrationPaths,
) -> io::Result<ConfigUpdateLock> {
    check_config_target(path)?;
    let target = resolve_target(path)?;
    let lock_path = config_update_lock_path(&target, paths)?;
    // No lock means no edit: a lock directory that cannot be created or a
    // lock that cannot be taken fails the change instead of editing unlocked.
    let lock = shepr_platform::ipc::acquire_flock_lock(&lock_path, true).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "could not lock {} for editing ({}): {error}",
                target.display(),
                lock_path.display()
            ),
        )
    })?;
    Ok(ConfigUpdateLock { _lock: lock })
}

fn config_update_lock_path(target: &Path, paths: &AgentIntegrationPaths) -> io::Result<PathBuf> {
    // Resolve an existing target or parent so two symlinked agent config
    // directories still key the same persistent lock file.
    let key = canonicalize_config_target(target)?;
    Ok(shepr_platform::ipc::keyed_lock_path(
        &paths.config_update_lock_dir()?,
        &key,
    ))
}

/// Resolves existing ancestors while allowing the target or its parent to be
/// absent before the first integration install.
fn canonicalize_config_target(target: &Path) -> io::Result<PathBuf> {
    let mut current = target;
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(current) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(file_name) = current.file_name() else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "integration config target has no file name",
                    ));
                };
                missing.push(PathBuf::from(file_name));
                current = match current
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                {
                    Some(parent) => parent,
                    None => Path::new("."),
                };
            }
            Err(error) => return Err(error),
        }
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
    for _ in 0..MAX_CONFIG_SYMLINK_DEPTH {
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
    inner: AtomicReplace,
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
        let inner = AtomicReplace::prepare_with_policy(
            &target,
            ".shepr-config",
            PermissionPolicy::UserConfig { existing },
            contents,
        )?;
        Ok(Self { inner })
    }

    fn commit(self) -> io::Result<()> {
        self.inner.commit_after(reject_hard_links)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
impl Replacement {
    fn temporary(&self) -> &Path {
        self.inner.temporary_path()
    }
}
