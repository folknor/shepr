use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct PrivateFilePolicyError {
    path: PathBuf,
    reason: PrivateFilePolicyReason,
}

#[derive(Debug)]
enum PrivateFilePolicyReason {
    NotRegular,
    WrongOwner { expected: u32, found: u32 },
    WrongMode { expected: u32, found: u32 },
}

impl fmt::Display for PrivateFilePolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.reason {
            PrivateFilePolicyReason::NotRegular => {
                write!(f, "{} is not a regular file", self.path.display())
            }
            PrivateFilePolicyReason::WrongOwner { expected, found } => write!(
                f,
                "{} must be owned by uid {expected}; found uid {found}",
                self.path.display()
            ),
            PrivateFilePolicyReason::WrongMode { expected, found } => write!(
                f,
                "{} must have mode {expected:04o}; found {found:04o}",
                self.path.display()
            ),
        }
    }
}

impl std::error::Error for PrivateFilePolicyError {}

/// Shared owner and file-type policy for files shepr trusts or writes.
pub(crate) struct PrivateFile;

impl PrivateFile {
    /// Gives open-time rejections the same policy error as post-open checks
    /// when the path currently names a non-regular object. Directories and
    /// symlinks can fail inside `open(2)` before callers can inspect the fd.
    pub(crate) fn normalize_open_error(path: &Path, error: std::io::Error) -> std::io::Error {
        if std::fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.is_file()) {
            Self::refusal(path, PrivateFilePolicyReason::NotRegular)
        } else {
            error
        }
    }

    pub(crate) fn require_owned_regular(
        path: &Path,
        metadata: &std::fs::Metadata,
        expected_uid: u32,
    ) -> std::io::Result<()> {
        let reason = Self::policy_reason(metadata, expected_uid, None);
        match reason {
            Some(reason) => Err(Self::refusal(path, reason)),
            None => Ok(()),
        }
    }

    pub(crate) fn is_owned_regular_with_mode(
        metadata: &std::fs::Metadata,
        expected_uid: u32,
        expected_mode: u32,
    ) -> bool {
        Self::policy_reason(metadata, expected_uid, Some(expected_mode)).is_none()
    }

    fn policy_reason(
        metadata: &std::fs::Metadata,
        expected_uid: u32,
        expected_mode: Option<u32>,
    ) -> Option<PrivateFilePolicyReason> {
        if !metadata.is_file() {
            Some(PrivateFilePolicyReason::NotRegular)
        } else if metadata.uid() != expected_uid {
            Some(PrivateFilePolicyReason::WrongOwner {
                expected: expected_uid,
                found: metadata.uid(),
            })
        } else if let Some(expected_mode) = expected_mode {
            let found = metadata.mode() & super::limits::PERMISSION_BITS;
            (found != expected_mode).then_some(PrivateFilePolicyReason::WrongMode {
                expected: expected_mode,
                found,
            })
        } else {
            None
        }
    }

    fn refusal(path: &Path, reason: PrivateFilePolicyReason) -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            PrivateFilePolicyError {
                path: path.to_path_buf(),
                reason,
            },
        )
    }
}

/// Why a directory failed [`require_private_directory`]: the filesystem could
/// not be read, or what it holds is not a private directory.
#[derive(Debug)]
pub enum PrivateDirError {
    Io(std::io::Error),
    /// Not a directory owned by the current user with the private mode, or a
    /// symlink.
    Policy,
}

/// Shared exact-mode policy for directories that contain private runtime state.
pub(crate) struct PrivateDir;

impl PrivateDir {
    pub(crate) fn require(path: &Path) -> Result<(), PrivateDirError> {
        let metadata = std::fs::symlink_metadata(path).map_err(PrivateDirError::Io)?;
        if Self::is_owned_private(&metadata, super::effective_uid()) {
            Ok(())
        } else {
            Err(PrivateDirError::Policy)
        }
    }

    pub(crate) fn is_private(path: &Path, expected_uid: u32) -> bool {
        std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| Self::is_owned_private(&metadata, expected_uid))
    }

    fn is_owned_private(metadata: &std::fs::Metadata, expected_uid: u32) -> bool {
        metadata.is_dir()
            && metadata.uid() == expected_uid
            && metadata.mode() & super::limits::PERMISSION_BITS
                == super::limits::PRIVATE_DIRECTORY_MODE
    }
}

/// Requires `path` to be a directory owned by the current user with the
/// private mode that is not a symlink, the policy for directories that hold
/// private runtime state.
pub fn require_private_directory(path: &Path) -> Result<(), PrivateDirError> {
    PrivateDir::require(path)
}

pub fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(super::limits::PRIVATE_FILE_MODE)
        .open(path)
}

pub fn sync_directory(directory: &Path) -> std::io::Result<()> {
    std::fs::File::open(directory)?.sync_all()
}

/// Opens `path` for reading only if the object it resolves to (symlinks
/// followed) is a regular file; otherwise returns that object's type. The path
/// is first pinned with `O_PATH`, which opens nothing for IO: a FIFO there does
/// not block and a device sees no open. The regular file is then reopened
/// through the pin, so what is read is the very object that was checked even
/// if the path changes meanwhile.
pub fn open_regular_file(path: &Path) -> std::io::Result<Result<std::fs::File, std::fs::FileType>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let pinned = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH)
        .open(path)?;
    let metadata = pinned.metadata()?;
    if !metadata.is_file() {
        return Ok(Err(metadata.file_type()));
    }
    // The path is known to exist now, so a failed reopen (no /proc in a
    // sandbox, say) must not read as NotFound: callers take that to mean the
    // file is absent and may then replace it without a backup.
    std::fs::File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))
        .map(Ok)
        .map_err(|error| {
            std::io::Error::other(format!(
                "could not reopen {} through /proc/self/fd: {error}",
                path.display()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn a_fifo_is_reported_without_blocking_and_a_file_is_read_through_its_pin() {
        let scratch = shepr_test_support::ScratchDir::new("open-regular-file");
        let fifo = scratch.path().join("fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("path");
        // SAFETY: a valid NUL-terminated path; mkfifo writes no memory of ours.
        assert_eq!(
            unsafe { libc::mkfifo(path.as_ptr(), super::super::limits::PRIVATE_FILE_MODE) },
            0
        );
        let file_type = open_regular_file(&fifo)
            .expect("the fifo is inspected")
            .expect_err("a fifo is not a regular file");
        assert!(std::os::unix::fs::FileTypeExt::is_fifo(&file_type));
        assert!(
            open_regular_file(scratch.path())
                .expect("the directory is inspected")
                .is_err()
        );

        let regular = scratch.path().join("regular");
        std::fs::write(&regular, b"content").expect("write");
        let mut text = String::new();
        open_regular_file(&regular)
            .expect("open")
            .expect("a regular file")
            .read_to_string(&mut text)
            .expect("read");
        assert_eq!(text, "content");
        assert_eq!(
            open_regular_file(&scratch.path().join("absent"))
                .expect_err("absent")
                .kind(),
            std::io::ErrorKind::NotFound
        );
    }
}
