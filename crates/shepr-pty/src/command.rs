//! Shepr-owned description of a process to launch inside a pane PTY.
//!
//! `PtyCommand` starts from the server's own environment and lets callers set,
//! remove, and inspect variables before spawn, so pane launch policy (terminal
//! identity, stripped host/agent variables, integration variables) is plain data
//! that tests can assert on without spawning anything.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path};

use shepr_core::env::{ChildEnv, EnvVar};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Program {
    /// An interactive pane shell. The configured name, environment fallback,
    /// executable and child `SHELL` are resolved together at spawn time.
    Shell { login: bool },
    /// Explicit argv; `argv[0]` is resolved against the command's `PATH`.
    Argv(Vec<OsString>),
}

#[derive(Clone, Copy)]
enum ShellResolutionPolicy {
    PaneProgram,
    ChildEnvironment,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtyCommand {
    program: Program,
    envs: BTreeMap<OsString, OsString>,
    cwd: Option<OsString>,
}

impl PtyCommand {
    /// Run `program` (argv\[0\]) with no further arguments yet.
    pub fn new<S: AsRef<OsStr>>(program: S) -> Self {
        Self::with_program(Program::Argv(vec![program.as_ref().to_owned()]))
    }

    /// Run the configured shell, or the environment/default shell, in pane mode.
    pub fn interactive_shell(default_shell: &str, login: bool) -> Self {
        let mut command = Self::with_program(Program::Shell { login });
        if !default_shell.trim().is_empty() {
            command.env(ChildEnv::Shell, default_shell.trim());
        }
        command
    }

    fn with_program(program: Program) -> Self {
        Self {
            program,
            envs: base_env(),
            cwd: None,
        }
    }

    pub fn is_login_shell(&self) -> bool {
        matches!(self.program, Program::Shell { login: true, .. })
    }

    /// Append an argument. An interactive shell takes no arguments; they are ignored.
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) {
        match &mut self.program {
            Program::Shell { .. } => {
                tracing::warn!("ignoring argument for interactive shell pty command");
            }
            Program::Argv(argv) => argv.push(arg.as_ref().to_owned()),
        }
    }

    pub fn args<I, S>(&mut self, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
    }

    pub fn env<K, V>(&mut self, key: K, value: V)
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.envs
            .insert(key.as_ref().to_owned(), value.as_ref().to_owned());
    }

    pub fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) {
        self.envs.remove(key.as_ref());
    }

    pub fn get_env<K: AsRef<OsStr>>(&self, key: K) -> Option<&OsStr> {
        self.envs.get(key.as_ref()).map(OsString::as_os_str)
    }

    /// Working directory. A missing or non-directory path falls back to a
    /// usable home directory, then `/` if no home directory is available.
    pub fn cwd<D: AsRef<OsStr>>(&mut self, dir: D) {
        self.cwd = Some(dir.as_ref().to_owned());
    }

    /// Build the `std::process::Command`: resolved program and argv0, working
    /// directory, and exactly this command's environment (the process
    /// environment is cleared first). PTY stdio and session setup are added by
    /// `crate::backend`.
    pub fn to_std_command(&self) -> io::Result<std::process::Command> {
        let dir: OsString = match self.cwd.as_ref() {
            Some(dir) if Path::new(dir).is_dir() => dir.clone(),
            requested => {
                let home = self.home_dir();
                if let Some(requested) = requested {
                    tracing::warn!(
                        cwd = %requested.to_string_lossy(),
                        fallback = %home.to_string_lossy(),
                        "pty working directory is not a directory; starting in the fallback"
                    );
                }
                home
            }
        };
        let (mut cmd, shell) = match &self.program {
            Program::Shell { login } => {
                let shell = self.resolve_shell(&dir, ShellResolutionPolicy::PaneProgram)?;
                let mut cmd = std::process::Command::new(&shell);
                if *login {
                    let basename = Path::new(&shell).file_name().unwrap_or(shell.as_os_str());
                    let mut argv0 = OsString::from("-");
                    argv0.push(basename);
                    cmd.arg0(argv0);
                }
                (cmd, shell)
            }
            Program::Argv(argv) => {
                let shell = self.resolve_shell(&dir, ShellResolutionPolicy::ChildEnvironment)?;
                let Some((program, args)) = argv.split_first() else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "pty command argv must not be empty",
                    ));
                };
                let resolved = self.search_path(program, &dir)?;
                let mut cmd = std::process::Command::new(resolved);
                cmd.arg0(program);
                cmd.args(args);
                (cmd, shell)
            }
        };
        cmd.current_dir(dir);
        cmd.env_clear();
        cmd.envs(&self.envs);
        // The child sees the same resolved `$SHELL` that was selected above.
        cmd.env(ChildEnv::Shell, shell);
        Ok(cmd)
    }

    /// Resolve `$SHELL` once for this launch. Pane shells use `/bin/sh` when
    /// the environment value is empty and reject an invalid selected shell;
    /// other child commands fall back to passwd, then `/bin/sh`.
    fn resolve_shell(&self, cwd: &OsStr, policy: ShellResolutionPolicy) -> io::Result<OsString> {
        let inherited = self.get_env(ChildEnv::Shell).and_then(trimmed_shell);
        let candidate = inherited.clone().unwrap_or_else(|| match policy {
            ShellResolutionPolicy::PaneProgram => OsString::from("/bin/sh"),
            ShellResolutionPolicy::ChildEnvironment => passwd_shell(),
        });
        match self.search_path(&candidate, cwd) {
            Ok(resolved) => Ok(resolved),
            Err(err) if matches!(policy, ShellResolutionPolicy::PaneProgram) => Err(err),
            Err(_) if inherited.is_none() => Ok(OsString::from("/bin/sh")),
            Err(err) => {
                if let Some(shell) = inherited {
                    tracing::warn!(
                        shell = %shell.to_string_lossy(),
                        err = %err,
                        "SHELL is not executable; falling back to passwd shell"
                    );
                }
                let fallback = passwd_shell();
                Ok(self
                    .search_path(&fallback, cwd)
                    .unwrap_or_else(|_| OsString::from("/bin/sh")))
            }
        }
    }

    fn home_dir(&self) -> OsString {
        if let Some(home) = self
            .get_env(EnvVar::Home)
            .filter(|home| Path::new(home).is_absolute() && Path::new(home).is_dir())
        {
            return home.to_owned();
        }
        passwd_field(|entry| entry.pw_dir.cast_const())
            .filter(|home| Path::new(home).is_absolute() && Path::new(home).is_dir())
            .unwrap_or_else(|| OsString::from("/"))
    }

    fn search_path(&self, exe: &OsStr, cwd: &OsStr) -> io::Result<OsString> {
        let exe_path = Path::new(exe);
        if exe_path.is_relative() {
            let cwd = Path::new(cwd);

            // An executable explicitly relative to cwd is only looked up there.
            if is_cwd_relative_path(exe_path) {
                let abs_path = cwd.join(exe_path);
                match classify_candidate(&abs_path) {
                    CandidateStatus::Executable => return Ok(abs_path.into_os_string()),
                    status => return Err(candidate_error(&abs_path, status)),
                }
            }

            let mut errors = Vec::new();
            if let Some(path) = self.get_env(ChildEnv::Path) {
                for dir in std::env::split_paths(path) {
                    let candidate = cwd.join(dir).join(exe_path);
                    let status = classify_candidate(&candidate);
                    match status {
                        CandidateStatus::Executable => return Ok(candidate.into_os_string()),
                        CandidateStatus::Directory | CandidateStatus::NotExecutable => {
                            errors.push(candidate_problem(&candidate, status));
                        }
                        CandidateStatus::Missing => {}
                    }
                }
                errors.push(format!("no viable candidates found in PATH {path:?}"));
            } else {
                errors.push("PATH is not set".to_string());
            }
            return Err(spawn_error(
                io::ErrorKind::NotFound,
                format!("{}: {}", exe_path.display(), errors.join("; ")),
            ));
        }

        match classify_candidate(exe_path) {
            CandidateStatus::Executable => Ok(exe.to_owned()),
            status => Err(candidate_error(exe_path, status)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateStatus {
    Executable,
    Directory,
    NotExecutable,
    Missing,
}

fn classify_candidate(path: &Path) -> CandidateStatus {
    if path.is_dir() {
        CandidateStatus::Directory
    } else if access_ok(path, libc::X_OK) {
        CandidateStatus::Executable
    } else if access_ok(path, libc::F_OK) {
        CandidateStatus::NotExecutable
    } else {
        CandidateStatus::Missing
    }
}

fn candidate_problem(path: &Path, status: CandidateStatus) -> String {
    match status {
        CandidateStatus::Directory => format!("{} exists but is a directory", path.display()),
        CandidateStatus::NotExecutable => {
            format!("{} exists but is not executable", path.display())
        }
        CandidateStatus::Executable | CandidateStatus::Missing => String::new(),
    }
}

fn candidate_error(path: &Path, status: CandidateStatus) -> io::Error {
    let (kind, detail) = match status {
        CandidateStatus::Directory => (
            io::ErrorKind::InvalidInput,
            format!("{} is a directory", path.display()),
        ),
        CandidateStatus::NotExecutable => (
            io::ErrorKind::PermissionDenied,
            format!("{} is not executable", path.display()),
        ),
        CandidateStatus::Missing => (
            io::ErrorKind::NotFound,
            format!("{} does not exist", path.display()),
        ),
        CandidateStatus::Executable => (
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

/// Trim whitespace from `$SHELL` without discarding valid non-UTF-8 path
/// bytes. Non-UTF-8 values only recognize ASCII whitespace at the edges.
fn trimmed_shell(shell: &OsStr) -> Option<OsString> {
    if let Some(shell) = shell.to_str() {
        let shell = shell.trim();
        return (!shell.is_empty()).then(|| OsString::from(shell));
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
    (start < end).then(|| OsString::from_vec(bytes[start..end].to_vec()))
}

/// The server's environment. Shell selection and validation happen at spawn,
/// after pane policy and launch environment have been applied.
#[expect(
    clippy::disallowed_methods,
    reason = "a pane child inherits the server's environment verbatim; it is copied, not interpreted, and pane launch policy then edits the copy"
)]
fn base_env() -> BTreeMap<OsString, OsString> {
    std::env::vars_os().collect()
}

fn passwd_shell() -> OsString {
    match passwd_field(|entry| entry.pw_shell.cast_const()) {
        Some(shell) if access_ok(Path::new(&shell), libc::X_OK) => shell,
        Some(shell) => {
            tracing::warn!(
                shell = %shell.to_string_lossy(),
                "passwd shell is not executable, falling back to /bin/sh"
            );
            OsString::from("/bin/sh")
        }
        None => OsString::from("/bin/sh"),
    }
}

/// Read one string field of the current user's passwd entry.
fn passwd_field(select: fn(&libc::passwd) -> *const libc::c_char) -> Option<OsString> {
    let mut buf: Vec<libc::c_char> = vec![0; 1024];
    loop {
        // SAFETY: an all-zero passwd value has null pointers and zero scalars,
        // all valid initial values for getpwuid_r to overwrite.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `entry`, `buf`, and `result` are writable values of the
        // sizes required by getpwuid_r; the function does not retain them.
        let status = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut entry,
                buf.as_mut_ptr(),
                buf.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buf.len() < 64 * 1024 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        let field = select(&entry);
        if field.is_null() {
            return None;
        }
        // SAFETY: after a successful getpwuid_r call, the selected field is a
        // NUL-terminated string inside the still-live result buffer.
        let bytes = unsafe { CStr::from_ptr(field) }.to_bytes();
        return Some(OsStr::from_bytes(bytes).to_owned());
    }
}

fn access_ok(path: &Path, mode: libc::c_int) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a live NUL-terminated CString for the duration of
    // access(2), which reads but does not retain its pointer.
    unsafe { libc::access(path.as_ptr(), mode) == 0 }
}

/// True if the path begins with `./` or `../`.
fn is_cwd_relative_path(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(Component::CurDir | Component::ParentDir)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_relative_paths_are_recognized() {
        assert!(is_cwd_relative_path(Path::new(".")));
        assert!(is_cwd_relative_path(Path::new("./foo")));
        assert!(is_cwd_relative_path(Path::new("../foo")));
        assert!(!is_cwd_relative_path(Path::new("foo")));
        assert!(!is_cwd_relative_path(Path::new("/foo")));
    }

    #[test]
    fn path_candidate_classification_distinguishes_all_filesystem_cases() {
        let scratch = shepr_test_support::ScratchDir::new("pty-candidates");
        let directory = scratch.join("directory");
        std::fs::create_dir(&directory).expect("test precondition");
        let executable = scratch.join("executable");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("test precondition");
        let not_executable = scratch.join("not-executable");
        std::fs::write(&not_executable, "content").expect("test precondition");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
                .expect("test precondition");
            std::fs::set_permissions(&not_executable, std::fs::Permissions::from_mode(0o644))
                .expect("test precondition");
        }

        assert_eq!(classify_candidate(&directory), CandidateStatus::Directory);
        assert_eq!(classify_candidate(&executable), CandidateStatus::Executable);
        assert_eq!(
            classify_candidate(&not_executable),
            CandidateStatus::NotExecutable
        );
        assert_eq!(
            classify_candidate(&scratch.join("missing")),
            CandidateStatus::Missing
        );
    }

    #[test]
    fn env_edits_are_visible_before_spawn() {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.env("SHEPR_PTY_TEST_KEY", "value");
        assert_eq!(cmd.get_env("SHEPR_PTY_TEST_KEY"), Some(OsStr::new("value")));
        cmd.env_remove("SHEPR_PTY_TEST_KEY");
        assert!(cmd.get_env("SHEPR_PTY_TEST_KEY").is_none());
        cmd.env_remove("SHELL");
        assert!(cmd.get_env("SHELL").is_none());
    }

    #[test]
    fn std_command_carries_exactly_the_command_env() {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.env("SHEPR_PTY_TEST_SET", "1");
        cmd.env("SHEPR_PTY_TEST_REMOVED", "1");
        cmd.env_remove("SHEPR_PTY_TEST_REMOVED");
        let std_cmd = cmd.to_std_command().expect("build std command");
        let envs: BTreeMap<OsString, Option<OsString>> = std_cmd
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
            .collect();
        assert_eq!(
            envs.get(OsStr::new("SHEPR_PTY_TEST_SET")),
            Some(&Some(OsString::from("1")))
        );
        assert!(!envs.contains_key(OsStr::new("SHEPR_PTY_TEST_REMOVED")));
    }

    #[test]
    fn login_shell_execs_shell_env_without_arguments() {
        let cmd = PtyCommand::interactive_shell("/bin/sh", true);
        let std_cmd = cmd.to_std_command().expect("build std command");
        assert_eq!(std_cmd.get_program(), OsStr::new("/bin/sh"));
        assert_eq!(std_cmd.get_args().count(), 0);
    }

    #[test]
    fn child_sees_resolved_shell_not_a_non_executable_shell_env() {
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.env("SHELL", "/__shepr_missing_shell__");
        let std_cmd = cmd.to_std_command().expect("build std command");
        let shell = std_cmd
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("SHELL"))
            .and_then(|(_, value)| value.map(OsStr::to_owned));
        assert_eq!(
            shell,
            Some(
                cmd.resolve_shell(&cmd.home_dir(), ShellResolutionPolicy::ChildEnvironment)
                    .expect("test precondition")
            )
        );
        assert_ne!(shell, Some(OsString::from("/__shepr_missing_shell__")));
    }

    #[test]
    fn home_fallback_requires_an_existing_absolute_directory() {
        let scratch = shepr_test_support::ScratchDir::new("pty-home");
        let mut cmd = PtyCommand::new("/bin/sh");
        cmd.env("HOME", scratch.path());
        assert_eq!(cmd.home_dir(), scratch.path().as_os_str());

        for unusable in [
            OsString::from("relative/home"),
            scratch.path().join("missing").into_os_string(),
        ] {
            cmd.env("HOME", &unusable);
            let home = cmd.home_dir();
            assert_ne!(home, unusable);
            assert!(Path::new(&home).is_absolute() && Path::new(&home).is_dir());
        }
    }

    #[test]
    fn missing_program_is_reported_before_spawn() {
        let cmd = PtyCommand::new("/__shepr_missing_program__");
        let err = cmd
            .to_std_command()
            .expect_err("missing program must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
