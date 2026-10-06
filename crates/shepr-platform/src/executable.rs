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

/// The shells a pane may run. Agent resume types a command into the pane's
/// shell quoted as POSIX words (`shepr_core::shell_quote`), which only the
/// POSIX family parses as written: nu has no `'\''` concatenation and csh
/// expands `!` inside single quotes. So config validation admits these alone.
const POSIX_SHELL_NAMES: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "mksh"];

/// Every shell detection recognises in a pane's process tree, as a process
/// name: pane-shell recognition, the generic-runtime ranking and `-c`
/// unwrapping. Each of these shells takes its command string as
/// `-c <command>`. Wider than [`POSIX_SHELL_NAMES`] because an agent may be
/// started through any shell inside a pane.
const SHELL_NAMES: &[&str] = &[
    "sh", "bash", "dash", "zsh", "fish", "ksh", "mksh", "csh", "tcsh", "elvish", "xonsh", "nu",
];

/// A process name (a path, or a login shell's `-`-prefixed argv0) reduced to
/// the bare program name.
fn bare_program_name(name: &str) -> &str {
    name.rsplit('/')
        .next()
        .unwrap_or(name)
        .trim_start_matches('-')
}

/// Whether a process name names a shell from [`SHELL_NAMES`].
pub fn is_shell_process_name(name: &str) -> bool {
    let normalized = bare_program_name(name);
    SHELL_NAMES
        .iter()
        .any(|shell| shell.eq_ignore_ascii_case(normalized))
}

/// Whether a program name names a shell a pane may run, from
/// [`POSIX_SHELL_NAMES`].
pub fn is_pane_shell_name(name: &str) -> bool {
    let normalized = bare_program_name(name);
    POSIX_SHELL_NAMES
        .iter()
        .any(|shell| shell.eq_ignore_ascii_case(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_process_names_reject_exec_replacement_programs() {
        for shell in ["bash", "-zsh", "/bin/fish", "nu"] {
            assert!(is_shell_process_name(shell), "{shell}");
        }
        for program in ["vim", "nvim", "cargo", "test-runner", "opencode"] {
            assert!(!is_shell_process_name(program), "{program}");
        }
    }

    #[test]
    fn pane_shells_are_the_posix_family_only() {
        for shell in POSIX_SHELL_NAMES {
            assert!(SHELL_NAMES.contains(shell), "{shell} is also recognised");
            assert!(is_pane_shell_name(shell), "{shell}");
        }
        for shell in [
            "fish",
            "csh",
            "tcsh",
            "elvish",
            "xonsh",
            "nu",
            "/usr/bin/fish",
        ] {
            assert!(!is_pane_shell_name(shell), "{shell}");
        }
    }
}
