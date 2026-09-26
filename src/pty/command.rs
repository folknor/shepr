//! Shepr-owned description of a process to launch inside a pane PTY.
//!
//! `PtyCommand` starts from the server's own environment and lets callers set,
//! remove, and inspect variables before spawn, so pane launch policy (terminal
//! identity, stripped host/agent variables, integration variables) is plain data
//! that tests can assert on without spawning anything.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Program {
    /// The user's shell run as a login shell: argv0 is `-<basename>`, and the
    /// shell path is taken from the command's `SHELL` at spawn time.
    LoginShell,
    /// Explicit argv; `argv[0]` is resolved against the command's `PATH`.
    Argv(Vec<OsString>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PtyCommand {
    program: Program,
    envs: BTreeMap<OsString, OsString>,
    cwd: Option<OsString>,
}

impl PtyCommand {
    /// Run `program` (argv\[0\]) with no further arguments yet.
    pub(crate) fn new<S: AsRef<OsStr>>(program: S) -> Self {
        Self::with_program(Program::Argv(vec![program.as_ref().to_owned()]))
    }

    /// Run the shell named by `SHELL` as a login shell (`-zsh` convention).
    pub(crate) fn login_shell() -> Self {
        Self::with_program(Program::LoginShell)
    }

    fn with_program(program: Program) -> Self {
        Self {
            program,
            envs: base_env(),
            cwd: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_login_shell(&self) -> bool {
        matches!(self.program, Program::LoginShell)
    }

    /// Explicit argv; empty for a login shell, whose argv is decided at spawn.
    #[cfg(test)]
    pub(crate) fn argv(&self) -> &[OsString] {
        match &self.program {
            Program::LoginShell => &[],
            Program::Argv(argv) => argv,
        }
    }

    /// Append an argument. A login shell takes no arguments; they are ignored.
    pub(crate) fn arg<S: AsRef<OsStr>>(&mut self, arg: S) {
        match &mut self.program {
            Program::LoginShell => {
                tracing::warn!("ignoring argument for login shell pty command");
            }
            Program::Argv(argv) => argv.push(arg.as_ref().to_owned()),
        }
    }

    pub(crate) fn args<I, S>(&mut self, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
    }

    pub(crate) fn env<K, V>(&mut self, key: K, value: V)
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.envs
            .insert(key.as_ref().to_owned(), value.as_ref().to_owned());
    }

    pub(crate) fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) {
        self.envs.remove(key.as_ref());
    }

    pub(crate) fn get_env<K: AsRef<OsStr>>(&self, key: K) -> Option<&OsStr> {
        self.envs.get(key.as_ref()).map(OsString::as_os_str)
    }

    /// Working directory. A missing or non-directory path falls back to `HOME`.
    pub(crate) fn cwd<D: AsRef<OsStr>>(&mut self, dir: D) {
        self.cwd = Some(dir.as_ref().to_owned());
    }

    /// Build the `std::process::Command`: resolved program and argv0, working
    /// directory, and exactly this command's environment (the process
    /// environment is cleared first). PTY stdio and session setup are added by
    /// `crate::pty::backend`.
    pub(crate) fn to_std_command(&self) -> io::Result<std::process::Command> {
        let home = self.home_dir();
        let dir: OsString = self
            .cwd
            .as_ref()
            .filter(|dir| Path::new(dir).is_dir())
            .cloned()
            .unwrap_or(home);
        let shell = self.shell();

        let mut cmd = match &self.program {
            Program::LoginShell => {
                let mut cmd = std::process::Command::new(&shell);
                let basename = shell.rsplit('/').next().unwrap_or(&shell);
                cmd.arg0(format!("-{basename}"));
                cmd
            }
            Program::Argv(argv) => {
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
                cmd
            }
        };

        cmd.current_dir(dir);
        cmd.env_clear();
        cmd.envs(&self.envs);
        // After `envs`, so the resolved shell (with its passwd / `/bin/sh`
        // fallback for a non-executable `SHELL`) is what the child sees.
        cmd.env("SHELL", shell);
        Ok(cmd)
    }

    /// `SHELL` when it names an executable, else the passwd shell, else `/bin/sh`.
    fn shell(&self) -> String {
        if let Some(shell) = self.get_env("SHELL").and_then(OsStr::to_str) {
            if access_ok(Path::new(shell), libc::X_OK) {
                return shell.to_owned();
            }
            tracing::warn!(
                shell = %shell,
                "$SHELL is not executable, falling back to password db lookup"
            );
        }
        passwd_shell()
    }

    fn home_dir(&self) -> OsString {
        if let Some(home) = self.get_env("HOME") {
            return home.to_owned();
        }
        passwd_field(|entry| entry.pw_dir.cast_const())
            .map(OsString::from)
            .unwrap_or_else(|| OsString::from("/"))
    }

    fn search_path(&self, exe: &OsStr, cwd: &OsStr) -> io::Result<OsString> {
        let exe_path = Path::new(exe);
        if exe_path.is_relative() {
            let cwd = Path::new(cwd);

            // An executable explicitly relative to cwd is only looked up there.
            if is_cwd_relative_path(exe_path) {
                let abs_path = cwd.join(exe_path);
                if abs_path.is_dir() {
                    return Err(spawn_error(
                        io::ErrorKind::InvalidInput,
                        format!("{} is a directory", abs_path.display()),
                    ));
                }
                if access_ok(&abs_path, libc::X_OK) {
                    return Ok(abs_path.into_os_string());
                }
                if access_ok(&abs_path, libc::F_OK) {
                    return Err(spawn_error(
                        io::ErrorKind::PermissionDenied,
                        format!("{} is not executable", abs_path.display()),
                    ));
                }
                return Err(spawn_error(
                    io::ErrorKind::NotFound,
                    format!("{} does not exist", abs_path.display()),
                ));
            }

            let mut errors = Vec::new();
            if let Some(path) = self.get_env("PATH") {
                for dir in std::env::split_paths(path) {
                    let candidate = cwd.join(dir).join(exe_path);
                    if candidate.is_dir() {
                        errors.push(format!("{} exists but is a directory", candidate.display()));
                    } else if access_ok(&candidate, libc::X_OK) {
                        return Ok(candidate.into_os_string());
                    } else if access_ok(&candidate, libc::F_OK) {
                        errors.push(format!(
                            "{} exists but is not executable",
                            candidate.display()
                        ));
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

        if exe_path.is_dir() {
            return Err(spawn_error(
                io::ErrorKind::InvalidInput,
                format!("{} is a directory", exe_path.display()),
            ));
        }
        if !access_ok(exe_path, libc::X_OK) {
            if access_ok(exe_path, libc::F_OK) {
                return Err(spawn_error(
                    io::ErrorKind::PermissionDenied,
                    format!("{} is not executable", exe_path.display()),
                ));
            }
            return Err(spawn_error(
                io::ErrorKind::NotFound,
                format!("{} does not exist", exe_path.display()),
            ));
        }
        Ok(exe.to_owned())
    }
}

fn spawn_error(kind: io::ErrorKind, mut detail: String) -> io::Error {
    detail.insert_str(0, "unable to spawn ");
    io::Error::new(kind, detail)
}

/// The server's environment, with `SHELL` filled from passwd when unset.
fn base_env() -> BTreeMap<OsString, OsString> {
    let mut env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    env.entry(OsString::from("SHELL"))
        .or_insert_with(|| OsString::from(passwd_shell()));
    env
}

fn passwd_shell() -> String {
    match passwd_field(|entry| entry.pw_shell.cast_const()) {
        Some(shell) if access_ok(Path::new(&shell), libc::X_OK) => shell,
        Some(shell) => {
            tracing::warn!(
                shell = %shell,
                "passwd shell is not executable, falling back to /bin/sh"
            );
            "/bin/sh".to_string()
        }
        None => "/bin/sh".to_string(),
    }
}

/// Read one string field of the current user's passwd entry.
fn passwd_field(select: fn(&libc::passwd) -> *const libc::c_char) -> Option<String> {
    let mut buf: Vec<libc::c_char> = vec![0; 1024];
    loop {
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
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
        // The field points into `buf`, which is still alive here.
        return unsafe { CStr::from_ptr(field) }
            .to_str()
            .ok()
            .map(str::to_owned);
    }
}

fn access_ok(path: &Path, mode: libc::c_int) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
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
    fn env_edits_are_visible_before_spawn() {
        let mut cmd = PtyCommand::new("/bin/sh");
        assert!(cmd.get_env("SHELL").is_some(), "SHELL is always seeded");
        cmd.env("SHEPR_PTY_TEST_KEY", "value");
        assert_eq!(cmd.get_env("SHEPR_PTY_TEST_KEY"), Some(OsStr::new("value")));
        cmd.env_remove("SHEPR_PTY_TEST_KEY");
        assert!(cmd.get_env("SHEPR_PTY_TEST_KEY").is_none());
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
        let mut cmd = PtyCommand::login_shell();
        cmd.env("SHELL", "/bin/sh");
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
        assert_eq!(shell, Some(OsString::from(cmd.shell())));
        assert_ne!(shell, Some(OsString::from("/__shepr_missing_shell__")));
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
