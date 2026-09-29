use std::fs;
use std::io;
use std::path::Path;

use super::atomic_replace::{AtomicReplace, PermissionPolicy};

/// Whether `path` is a regular file (following symlinks). Absence is `false`;
/// any other stat error (`EACCES`, `ELOOP`) is returned, not read as absence.
pub(crate) fn is_file(path: &Path) -> io::Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Whether `path` is a directory (following symlinks). Absence is `false`;
/// any other stat error is returned, not read as absence.
pub(crate) fn is_dir(path: &Path) -> io::Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// The contents of `path` when it is a regular file, `None` when nothing
/// (or a non-file) is there. Stat and read errors are returned.
pub(crate) fn read_if_file(path: &Path) -> io::Result<Option<String>> {
    if is_file(path)? {
        fs::read_to_string(path).map(Some)
    } else {
        Ok(None)
    }
}

pub(crate) fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
}

/// Install a shepr-managed asset (hook script, plugin file) by writing a
/// sibling temporary file and renaming it over `path`.
///
/// Never truncate-and-rewrite in place: agents run hook scripts through
/// `bash`, which reads a script incrementally while executing it, so a hook
/// that is running during a reinstall would otherwise continue from an
/// arbitrary byte offset of the new contents. The rename gives the new
/// contents a new inode; a running interpreter keeps reading the old one.
///
/// A symlink at `path` is replaced by the regular file, not followed; the
/// asset paths are shepr-owned.
pub(crate) fn write_managed_asset(
    path: &Path,
    contents: &[u8],
    executable: bool,
) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} has no file name", path.display())))?
        .to_string_lossy()
        .into_owned();

    let replacement = AtomicReplace::prepare_with_policy(
        path,
        &format!(".{name}.shepr"),
        PermissionPolicy::ManagedAsset { executable },
        contents,
    )?;
    replacement.commit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::ScratchDir;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn managed_asset_replaces_the_inode_instead_of_rewriting_in_place() {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;

        let dir = ScratchDir::new("inode");
        let path = dir.join("hook.sh");
        write_managed_asset(&path, b"old contents\n", true).expect("first write");
        let old_inode = fs::metadata(&path).expect("metadata").ino();
        // A reader that opened the old script (as a running bash would) keeps
        // seeing the old bytes after the reinstall.
        let mut reader = fs::File::open(&path).expect("open old");

        write_managed_asset(&path, b"new contents, longer than before\n", true)
            .expect("second write");

        let mut old = String::new();
        reader.read_to_string(&mut old).expect("read old");
        assert_eq!(old, "old contents\n");
        assert_ne!(fs::metadata(&path).expect("metadata").ino(), old_inode);
        assert_eq!(
            fs::read_to_string(&path).expect("read new"),
            "new contents, longer than before\n"
        );
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o755
        );
        let leftovers = fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn managed_asset_write_failure_leaves_no_temporary_file() {
        let dir = ScratchDir::new("failure");
        // Renaming a file over a non-empty directory fails.
        let path = dir.join("occupied");
        fs::create_dir_all(path.join("child")).expect("test precondition");
        assert!(write_managed_asset(&path, b"x", false).is_err());
        let entries = fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .count();
        assert_eq!(entries, 1);
    }
}
