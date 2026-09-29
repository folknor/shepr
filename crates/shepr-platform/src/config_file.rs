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
        preserve_config_owner_with(
            (metadata.uid(), metadata.gid()),
            (current.uid(), current.gid()),
            || {
                // Keep ownership before restoring mode/ACLs; chown can clear mode bits.
                // SAFETY: fchown(2) on an fd `output` keeps open; integers only.
                if unsafe { libc::fchown(output.as_raw_fd(), metadata.uid(), metadata.gid()) } != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            },
        )?;
        // Replace inherited ACLs before enabling the original mode. Prepare all
        // required access controls while the temporary is empty, before writing
        // secrets.
        copy_config_xattrs(input.as_raw_fd(), output.as_raw_fd())?;
        output.set_permissions(metadata.permissions())?;
    }
    output.write_all(contents)?;
    output.sync_all()
}

fn preserve_config_owner_with(
    source: (libc::uid_t, libc::gid_t),
    current: (libc::uid_t, libc::gid_t),
    set_owner: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    if source == current {
        return Ok(());
    }
    if let Err(error) = set_owner() {
        // Replacing a writable file owned by another uid can still be valid
        // when the caller cannot reproduce that ownership.
        if error.raw_os_error() != Some(libc::EPERM) {
            return Err(error);
        }
        tracing::debug!(
            uid = source.0,
            gid = source.1,
            %error,
            "could not preserve config file ownership"
        );
    }
    Ok(())
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
    let destination_names = names(destination)?;
    for bytes in destination_names.split_inclusive(|byte| *byte == 0) {
        let name = CStr::from_bytes_with_nul(bytes).map_err(std::io::Error::other)?;
        if !is_posix_acl_xattr(name)
            || source_names
                .split_inclusive(|byte| *byte == 0)
                .any(|source_name| source_name == bytes)
        {
            continue;
        }
        // A parent directory can give the temporary an ACL the original file
        // did not have. Removing that inherited ACL is required to keep the
        // original access rules; unrelated inherited labels are left to the
        // filesystem's policy.
        // SAFETY: `name` is NUL-terminated and outlives the call.
        if unsafe { libc::fremovexattr(destination, name.as_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    for bytes in source_names.split_inclusive(|byte| *byte == 0) {
        let name = CStr::from_bytes_with_nul(bytes).map_err(std::io::Error::other)?;
        let required = is_posix_acl_xattr(name);
        let original = match value(source, name) {
            Ok(original) => original,
            Err(error) if required => return Err(error),
            Err(_) => continue,
        };
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
            let error = std::io::Error::last_os_error();
            if required {
                return Err(error);
            }
        }
    }
    Ok(())
}

// This required-copy policy covers Linux POSIX ACL xattrs only. Other ACL
// families and security labels use separate namespaces and remain best-effort
// here; this helper does not promise to preserve them when the filesystem
// refuses a copy.
fn is_posix_acl_xattr(name: &std::ffi::CStr) -> bool {
    name.to_bytes().starts_with(b"system.posix_acl_")
}

#[cfg(test)]
mod tests {
    use super::preserve_config_owner_with;

    #[test]
    fn permission_denied_preserving_config_owner_is_tolerated() {
        let mut attempted = false;
        let result = preserve_config_owner_with((1001, 1002), (1000, 1000), || {
            attempted = true;
            Err(std::io::Error::from_raw_os_error(libc::EPERM))
        });

        assert!(attempted, "the ownership operation should be tried");
        assert!(
            result.is_ok(),
            "EPERM must not prevent replacing the config"
        );
    }
}
