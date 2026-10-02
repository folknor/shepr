use std::path::Path;

pub fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
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
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
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
