use std::fs;
use std::io::{self, Read};
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

/// Missing config is empty; every existing non-regular object is an error.
/// Pin the object before reading so a FIFO never blocks and a concurrent path
/// replacement cannot swap a checked regular file for a device or pipe.
pub(super) fn read_config_bytes(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut file = match shepr_platform::open_regular_file(path) {
        Ok(Ok(file)) => file,
        Ok(Err(_)) => return Err(io::Error::other(NotRegularFile(path.to_path_buf()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("cannot read {}: {error}", path.display()),
            ));
        }
    };
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    Ok(Some(contents))
}

#[derive(Debug)]
pub(super) struct NotRegularFile(pub(super) std::path::PathBuf);

impl std::fmt::Display for NotRegularFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "cannot read {}: config is not a regular file",
            self.0.display()
        )
    }
}

impl std::error::Error for NotRegularFile {}

pub(crate) fn read_if_file(path: &Path) -> io::Result<Option<String>> {
    read_config_bytes(path)?
        .map(|bytes| {
            String::from_utf8(bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        })
        .transpose()
}

/// Install a shepr-managed asset (hook script, plugin file) by writing a
/// sibling temporary file and renaming it over `path`.
///
/// Never truncate-and-rewrite in place: agents run hook scripts through
/// `sh`, which reads a script incrementally while executing it, so a hook
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
    if path.file_name().is_none() {
        return Err(io::Error::other(format!(
            "{} has no file name",
            path.display()
        )));
    }

    let replacement = AtomicReplace::prepare_with_policy(
        path,
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
    fn config_reader_follows_files_and_distinguishes_absence_from_nonregular_objects() {
        use std::os::unix::fs::symlink;

        let dir = ScratchDir::new("integration-config-reader");
        let missing = dir.join("missing");
        let regular = dir.join("regular");
        let link = dir.join("link");
        let dangling = dir.join("dangling");
        let directory = dir.join("directory");
        let fifo = dir.join("fifo");
        let socket = dir.join("socket");
        fs::write(&regular, "preferences").expect("write config");
        symlink(&regular, &link).expect("link config");
        symlink(&missing, &dangling).expect("link absent config");
        fs::create_dir(&directory).expect("create directory");
        let fifo_name =
            std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("FIFO path");
        // SAFETY: the path is NUL-terminated and mkfifo retains no pointer.
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        let _socket = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");

        assert_eq!(
            read_if_file(&regular).expect("read config"),
            Some("preferences".to_string())
        );
        assert_eq!(
            read_if_file(&link).expect("read linked config"),
            Some("preferences".to_string())
        );
        assert_eq!(read_if_file(&missing).expect("absent config"), None);
        assert_eq!(read_if_file(&dangling).expect("dangling config"), None);
        for path in [&directory, &fifo, &socket] {
            let error = read_if_file(path)
                .expect_err("non-regular config must fail without opening for IO");
            assert!(
                error
                    .get_ref()
                    .is_some_and(<dyn std::error::Error + Send + Sync>::is::<NotRegularFile>)
            );
            assert!(
                read_config_bytes(path).is_err(),
                "snapshot uses the same policy"
            );
        }
    }

    #[test]
    fn managed_asset_replaces_the_inode_instead_of_rewriting_in_place() {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;

        let dir = ScratchDir::new("inode");
        let path = dir.join("hook.sh");
        write_managed_asset(&path, b"old contents\n", true).expect("first write");
        let old_inode = fs::metadata(&path).expect("metadata").ino();
        // A reader that opened the old script (as a running sh would) keeps
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
