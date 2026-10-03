//! Shared executable lookup for configured shells and PTY programs.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// How one candidate path looks to the platform-specific executable check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutableStatus {
    /// The caller's platform check classifies the candidate as executable.
    Executable,
    /// The candidate is a directory.
    Directory,
    /// The candidate exists but is not executable.
    NotExecutable,
    /// The candidate does not exist.
    Missing,
    /// The candidate could not be inspected.
    Uninspectable(io::ErrorKind),
}

/// Resolve a program path using the pane's `PATH` and working directory.
///
/// The caller supplies the platform-specific executability check, so path
/// lookup does not own platform access checks.
pub fn resolve_executable(
    program: &OsStr,
    path: Option<&OsStr>,
    cwd: &Path,
    mut classify: impl FnMut(&Path) -> ExecutableStatus,
) -> io::Result<PathBuf> {
    let program_path = Path::new(program);
    if program_path.is_relative() {
        if program.as_bytes().contains(&b'/') {
            let candidate = cwd.join(program_path);
            return match classify(&candidate) {
                ExecutableStatus::Executable => Ok(candidate),
                status => Err(candidate_error(&candidate, status)),
            };
        }

        let mut problems = Vec::new();
        if let Some(path) = path {
            for directory in std::env::split_paths(path) {
                let candidate = cwd.join(directory).join(program_path);
                let status = classify(&candidate);
                match status {
                    ExecutableStatus::Executable => return Ok(candidate),
                    ExecutableStatus::Directory
                    | ExecutableStatus::NotExecutable
                    | ExecutableStatus::Uninspectable(_) => {
                        problems.push(candidate_problem(&candidate, status));
                    }
                    ExecutableStatus::Missing => {}
                }
            }
            problems.push(format!("no viable candidates found in PATH {path:?}"));
        } else {
            problems.push("PATH is not set".to_owned());
        }
        return Err(spawn_error(
            io::ErrorKind::NotFound,
            format!("{}: {}", program_path.display(), problems.join("; ")),
        ));
    }

    match classify(program_path) {
        ExecutableStatus::Executable => Ok(program_path.to_path_buf()),
        status => Err(candidate_error(program_path, status)),
    }
}

/// Trim shell whitespace without discarding valid non-UTF-8 path bytes.
/// Non-UTF-8 values only recognize ASCII whitespace at the edges.
pub fn trim_shell_value(shell: &OsStr) -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    if let Some(shell) = shell.to_str() {
        let shell = shell.trim();
        return (!shell.is_empty()).then(|| shell.into());
    }

    let bytes = shell.as_bytes();
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    (start < end).then(|| std::ffi::OsString::from_vec(bytes[start..end].to_vec()))
}

fn candidate_problem(path: &Path, status: ExecutableStatus) -> String {
    match status {
        ExecutableStatus::Directory => format!("{} exists but is a directory", path.display()),
        ExecutableStatus::NotExecutable => {
            format!("{} exists but is not executable", path.display())
        }
        ExecutableStatus::Uninspectable(kind) => {
            format!("{} cannot be inspected: {kind}", path.display())
        }
        ExecutableStatus::Executable | ExecutableStatus::Missing => String::new(),
    }
}

fn candidate_error(path: &Path, status: ExecutableStatus) -> io::Error {
    let (kind, detail) = match status {
        ExecutableStatus::Directory => (
            io::ErrorKind::InvalidInput,
            format!("{} is a directory", path.display()),
        ),
        ExecutableStatus::NotExecutable => (
            io::ErrorKind::PermissionDenied,
            format!("{} is not executable", path.display()),
        ),
        ExecutableStatus::Missing => (
            io::ErrorKind::NotFound,
            format!("{} does not exist", path.display()),
        ),
        ExecutableStatus::Uninspectable(kind) => (
            kind,
            format!("{} cannot be inspected: {kind}", path.display()),
        ),
        ExecutableStatus::Executable => (
            io::ErrorKind::InvalidInput,
            format!("{} unexpectedly classified as executable", path.display()),
        ),
    };
    spawn_error(kind, detail)
}

fn spawn_error(kind: io::ErrorKind, mut detail: String) -> io::Error {
    detail.insert_str(0, "unable to spawn ");
    io::Error::new(kind, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_program_with_slash_resolves_from_the_working_directory() {
        let cwd = Path::new("/work");
        let expected = cwd.join("bin/zsh");
        let resolved = resolve_executable(
            OsStr::new("bin/zsh"),
            Some(OsStr::new("/tools")),
            cwd,
            |candidate| {
                if candidate == expected {
                    ExecutableStatus::Executable
                } else {
                    ExecutableStatus::Missing
                }
            },
        )
        .expect("a slash-containing relative path is resolved from cwd");

        assert_eq!(resolved, expected);
    }

    #[test]
    fn bare_program_name_is_searched_on_path() {
        let cwd = Path::new("/work");
        let expected = cwd.join("tools/zsh");
        let resolved = resolve_executable(
            OsStr::new("zsh"),
            Some(OsStr::new("tools:/other")),
            cwd,
            |candidate| {
                if candidate == expected {
                    ExecutableStatus::Executable
                } else {
                    ExecutableStatus::Missing
                }
            },
        )
        .expect("a bare program name is searched on PATH");

        assert_eq!(resolved, expected);
    }
}
