use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Whether `access(2)` permits the current process to execute `path`.
pub fn has_execute_access(path: &Path) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a live NUL-terminated CString for the duration of
    // access(2), which reads but does not retain its pointer.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}
