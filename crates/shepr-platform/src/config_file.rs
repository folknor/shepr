use std::{
    io::Write,
    os::fd::{AsRawFd, RawFd},
    path::Path,
};

pub fn config_file_link_count(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.nlink())
}

pub fn create_config_temporary(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o666)
        .open(path)
}

pub fn write_config_temporary(
    source: Option<&Path>,
    temporary: &Path,
    contents: &[u8],
) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(temporary)?;
    if let Some(source) = source {
        let input = std::fs::File::open(source)?;
        let metadata = input.metadata()?;
        let current = output.metadata()?;
        if (metadata.uid(), metadata.gid()) != (current.uid(), current.gid()) {
            // Keep ownership before restoring mode/ACLs; chown can clear mode bits.
            // SAFETY: fchown(2) on an fd `output` keeps open; integers only.
            if unsafe { libc::fchown(output.as_raw_fd(), metadata.uid(), metadata.gid()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        // Replace inherited ACLs before enabling the original mode. Prepare all
        // access controls while the temporary is empty, before writing secrets.
        copy_config_xattrs(input.as_raw_fd(), output.as_raw_fd())?;
        output.set_permissions(metadata.permissions())?;
    }
    output.write_all(contents)?;
    output.sync_all()
}

// Access ACLs and security labels live in xattrs on Linux. Mode bits alone can
// silently broaden access, especially with a default ACL on the parent directory.
fn copy_config_xattrs(source: RawFd, destination: RawFd) -> std::io::Result<()> {
    use std::ffi::CStr;
    fn names(fd: RawFd) -> std::io::Result<Vec<u8>> {
        // SAFETY: a null buffer of size 0 asks flistxattr(2) for the size
        // only; nothing is written.
        let size = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0) };
        if size < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOTSUP) {
                return Ok(Vec::new());
            }
            return Err(error);
        }
        let size = usize::try_from(size).unwrap_or(0);
        let mut buffer = vec![0; size];
        // SAFETY: writes at most `buffer.len()` bytes into the live buffer.
        let read = unsafe { libc::flistxattr(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 {
            return Err(std::io::Error::last_os_error());
        }
        buffer.truncate(usize::try_from(read).unwrap_or(0));
        Ok(buffer)
    }
    fn value(fd: RawFd, name: &CStr) -> std::io::Result<Vec<u8>> {
        // SAFETY: `name` is NUL-terminated; a null buffer of size 0 asks for
        // the size only.
        let size = unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let size = usize::try_from(size).unwrap_or(0);
        let mut buffer = vec![0; size];
        // SAFETY: `name` is NUL-terminated; writes at most `buffer.len()`
        // bytes into the live buffer.
        let read =
            unsafe { libc::fgetxattr(fd, name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 {
            return Err(std::io::Error::last_os_error());
        }
        buffer.truncate(usize::try_from(read).unwrap_or(0));
        Ok(buffer)
    }
    let source_names = names(source)?;
    for bytes in names(destination)?.split_inclusive(|byte| *byte == 0) {
        if !source_names
            .split_inclusive(|byte| *byte == 0)
            .any(|name| name == bytes)
        {
            let name = CStr::from_bytes_with_nul(bytes).map_err(std::io::Error::other)?;
            // SAFETY: `name` is NUL-terminated and outlives the call.
            if unsafe { libc::fremovexattr(destination, name.as_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    for bytes in source_names.split_inclusive(|byte| *byte == 0) {
        let name = CStr::from_bytes_with_nul(bytes).map_err(std::io::Error::other)?;
        let original = value(source, name)?;
        // Avoid requiring relabel privileges when the inherited label already matches.
        if value(destination, name).is_ok_and(|current| current == original) {
            continue;
        }
        // SAFETY: `name` is NUL-terminated; fsetxattr reads `original.len()`
        // bytes from the live buffer.
        if unsafe {
            libc::fsetxattr(
                destination,
                name.as_ptr(),
                original.as_ptr().cast(),
                original.len(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
