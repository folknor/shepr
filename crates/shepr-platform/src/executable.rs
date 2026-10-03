use std::ffi::CString;
use std::io;
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

/// How one candidate path for a program looks to this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutableStatus {
    /// `access(2)` permits executing the candidate.
    Executable,
    /// The candidate is a directory.
    Directory,
    /// The candidate exists but is not executable.
    NotExecutable,
    /// The candidate, or a directory on its path, does not exist.
    Missing,
    /// The candidate could not be inspected.
    Uninspectable(io::ErrorKind),
}

/// Classify `path` with the same `access(2)` check the PTY launch makes, so
/// noexec mounts and access policy are seen before a child is forked.
pub fn classify_executable(path: &Path) -> ExecutableStatus {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => ExecutableStatus::Directory,
        Ok(_) if has_execute_access(path) => ExecutableStatus::Executable,
        Ok(_) => ExecutableStatus::NotExecutable,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            ExecutableStatus::Missing
        }
        Err(error) => ExecutableStatus::Uninspectable(error.kind()),
    }
}

/// Every shell shepr recognises, as a process name. One list serves the pane
/// shell check in config validation and, in detection, pane-shell
/// recognition, the generic-runtime ranking and `-c` unwrapping: each of these
/// shells takes its command string as `-c <command>`.
const SHELL_NAMES: &[&str] = &[
    "sh", "bash", "dash", "zsh", "fish", "ksh", "mksh", "csh", "tcsh", "elvish", "xonsh", "nu",
];

/// Whether a process name (a path, or a login shell's `-`-prefixed argv0) names
/// a shell from [`SHELL_NAMES`].
pub fn is_pane_shell_process_name(name: &str) -> bool {
    let normalized = name
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .trim_start_matches('-');
    SHELL_NAMES
        .iter()
        .any(|shell| shell.eq_ignore_ascii_case(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_shell_process_names_reject_exec_replacement_programs() {
        for shell in ["bash", "-zsh", "/bin/fish"] {
            assert!(is_pane_shell_process_name(shell), "{shell}");
        }
        for program in ["vim", "nvim", "cargo", "test-runner", "opencode"] {
            assert!(!is_pane_shell_process_name(program), "{program}");
        }
    }
}
