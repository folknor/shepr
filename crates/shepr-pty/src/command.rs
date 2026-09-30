//! Shepr-owned description of the shell to launch inside a pane PTY.
//!
//! `PtyCommand` starts from the server's own environment (see `base_env` for
//! why it is inherited whole) and lets callers set, remove, and inspect
//! variables before spawn, so pane launch policy (terminal identity, stripped
//! host and agent variables, integration variables) is plain data that tests
//! can assert on without spawning anything.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use crate::limits::{
    PASSWD_BUFFER_GROWTH_FACTOR, PASSWD_BUFFER_INITIAL_BYTES, PASSWD_BUFFER_MAX_BYTES,
};
use shepr_core::env::{ChildEnv, EnvVar};
use shepr_core::shell::ExecutableStatus as CandidateStatus;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtyCommand {
    program: OsString,
    login: bool,
    envs: BTreeMap<OsString, OsString>,
    cwd: Option<OsString>,
    cwd_required: bool,
}

impl PtyCommand {
    /// Run the shell selected and resolved while the server loaded its config.
    pub fn interactive_shell(default_shell: &str, login: bool) -> Self {
        let default_shell = default_shell.trim();
        let mut command = Self {
            program: default_shell.into(),
            login,
            envs: base_env(),
            cwd: None,
            cwd_required: false,
        };
        command.env(ChildEnv::Shell, default_shell);
        command
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

    /// Working directory. Unless [`Self::require_cwd`] is set, a missing or
    /// unusable path falls back to a usable `HOME`, then the passwd home, then
    /// `/`.
    pub fn cwd<D: AsRef<OsStr>>(&mut self, dir: D) {
        self.cwd = Some(dir.as_ref().to_owned());
    }

    /// Use the requested working directory without checking it while building
    /// the command. If it disappeared, child spawn fails instead of starting
    /// in `HOME`. Restored agent commands use this so they cannot run in a
    /// different directory after their worker-side check.
    pub fn require_cwd(&mut self) {
        self.cwd_required = true;
    }

    /// Build the `std::process::Command`: resolved program and argv0, working
    /// directory, and exactly this command's environment (the process
    /// environment is cleared first). By default, if a requested cwd became
    /// unusable after validation, it starts in a usable `HOME`, then the
    /// passwd home, then `/`. A required cwd is passed to child spawn without
    /// checking it here. PTY stdio and session setup are added by
    /// `crate::backend`.
    pub(crate) fn to_std_command(&self) -> io::Result<std::process::Command> {
        let dir: OsString = if self.cwd_required {
            self.cwd.clone().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "required pty working directory was not set",
                )
            })?
        } else {
            match self.cwd.as_ref() {
                Some(dir) if usable_directory(Path::new(dir), "pty working directory") => {
                    dir.clone()
                }
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
            }
        };
        let shell = self.search_path(&self.program, &dir)?;
        let mut cmd = command_in(&shell, &dir);
        if self.login {
            let basename = Path::new(&shell).file_name().unwrap_or(shell.as_os_str());
            let mut argv0 = OsString::from("-");
            argv0.push(basename);
            cmd.arg0(argv0);
        }
        cmd.env_clear();
        cmd.envs(&self.envs);
        // `PWD` belongs to the pane's working directory, which can differ from
        // the server's; retain its logical path when absolute and let the
        // shell reconstruct it for relative paths. `OLDPWD` has no pane-local
        // history yet, so never inherit the server's previous directory.
        if Path::new(&dir).is_absolute() {
            cmd.env("PWD", &dir);
        } else {
            cmd.env_remove("PWD");
        }
        cmd.env_remove("OLDPWD");
        // The child sees the same resolved `$SHELL` that was selected above.
        cmd.env(ChildEnv::Shell, shell);
        Ok(cmd)
    }

    fn home_dir(&self) -> OsString {
        let usable_home =
            |home: &Path| home.is_absolute() && usable_directory(home, "home directory");
        // Apply the shared policy to the command's captured HOME. A stale cwd
        // is recoverable, so unusable HOME falls through to passwd and `/`.
        if let Ok(home) = shepr_core::pathutil::home_dir_from_env_value(self.get_env(EnvVar::Home))
            && usable_home(&home)
        {
            return home.into_os_string();
        }
        passwd_field(|entry| entry.pw_dir.cast_const())
            .filter(|home| usable_home(Path::new(home)))
            .unwrap_or_else(|| OsString::from("/"))
    }

    fn search_path(&self, exe: &OsStr, cwd: &OsStr) -> io::Result<OsString> {
        // Config validation also uses the shared resolver, which decides
        // between a cwd-relative path and a PATH lookup. Keep that decision
        // there so validation and launch cannot diverge.
        shepr_core::shell::resolve_executable(
            exe,
            self.get_env(ChildEnv::Path),
            Path::new(cwd),
            classify_candidate,
        )
        .map(PathBuf::into_os_string)
    }
}

/// The launch command for `program`, run in `dir`.
#[expect(
    clippy::disallowed_methods,
    reason = "a pane child starts in the pane's resolved working directory (or its home fallback), \
              set on the next line, never in the server's"
)]
fn command_in(program: &OsStr, dir: &OsStr) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command.current_dir(dir);
    command
}

/// Whether `path` is a directory (following symlinks). Absence, and a
/// non-directory anywhere along the path, are `Ok(false)`; any other stat
/// failure is the error, never read as absence.
fn is_directory(path: &Path) -> io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

/// [`is_directory`] for a path that has a fallback: a stat failure is logged
/// as itself and the path is not used.
fn usable_directory(path: &Path, what: &str) -> bool {
    is_directory(path).unwrap_or_else(|err| {
        tracing::warn!(
            path = %path.display(),
            error = %err,
            "{what} cannot be inspected; not using it"
        );
        false
    })
}

fn classify_candidate(path: &Path) -> CandidateStatus {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => CandidateStatus::Directory,
        Ok(_) if access_ok(path, libc::X_OK) => CandidateStatus::Executable,
        Ok(_) => CandidateStatus::NotExecutable,
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            CandidateStatus::Missing
        }
        Err(err) => CandidateStatus::Uninspectable(err.kind()),
    }
}

/// The server's environment, copied whole. A pane is the user's shell, so it
/// inherits what the session that started the server set up, as a shell under
/// tmux does: the agent socket, the display, the locale, `PATH`
/// additions and whatever else the user's login exports. shepr cannot know that
/// set, so an allowlist would silently break tools that read variables it never
/// heard of. What must not reach a pane is the smaller, closed set shepr does
/// know (its own handoffs, the outer terminal's identity, an outer agent
/// session's markers), and the pane launch layer removes exactly those, one
/// decision per registered variable. Shell selection and validation happen at
/// spawn, after that policy and the launch environment have been applied.
#[expect(
    clippy::disallowed_methods,
    reason = "a pane child inherits the server's environment verbatim; it is copied, not interpreted, and pane launch policy then edits the copy"
)]
fn base_env() -> BTreeMap<OsString, OsString> {
    std::env::vars_os().collect()
}

/// Read one string field of the current user's passwd entry.
fn passwd_field(select: fn(&libc::passwd) -> *const libc::c_char) -> Option<OsString> {
    let mut buf: Vec<libc::c_char> = vec![0; PASSWD_BUFFER_INITIAL_BYTES];
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
        if status == libc::ERANGE && buf.len() < PASSWD_BUFFER_MAX_BYTES {
            buf.resize(buf.len() * PASSWD_BUFFER_GROWTH_FACTOR, 0);
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
    // This is the local twin of `shepr_platform::has_execute_access`; the
    // PTY dependency policy keeps `shepr-platform` out of this crate.
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a live NUL-terminated CString for the duration of
    // access(2), which reads but does not retain its pointer.
    unsafe { libc::access(path.as_ptr(), mode) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::fixture;

    fn fixture_shell(steps: &[fixture::Step]) -> PtyCommand {
        let scratch = shepr_test_support::ScratchDir::new("pty-command-fixture");
        let shell = fixture::stand_in(scratch.path(), "shepr-fixture", steps);
        PtyCommand::interactive_shell(shell.to_str().expect("fixture path is UTF-8"), false)
    }

    #[test]
    fn path_candidate_classification_distinguishes_all_filesystem_cases() {
        let scratch = shepr_test_support::ScratchDir::new("pty-candidates");
        let directory = scratch.join("directory");
        std::fs::create_dir(&directory).expect("test precondition");
        let executable = scratch.join("executable");
        // Never run: classification checks access without executing the file.
        std::fs::write(&executable, "content").expect("test precondition");
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
        let mut cmd = PtyCommand::interactive_shell(fixture::path_str(), false);
        cmd.env("SHEPR_PTY_TEST_KEY", "value");
        assert_eq!(cmd.get_env("SHEPR_PTY_TEST_KEY"), Some(OsStr::new("value")));
        cmd.env_remove("SHEPR_PTY_TEST_KEY");
        assert!(cmd.get_env("SHEPR_PTY_TEST_KEY").is_none());
        cmd.env_remove("SHELL");
        assert!(cmd.get_env("SHELL").is_none());
    }

    #[test]
    fn std_command_carries_exactly_the_command_env() {
        let mut cmd = PtyCommand::interactive_shell(fixture::path_str(), false);
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
    fn std_command_sets_pwd_to_pane_cwd_and_drops_server_oldpwd() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("pty-command-cwd-env");
        let mut cmd = PtyCommand::interactive_shell(fixture::path_str(), false);
        cmd.cwd(scratch.path());
        cmd.env("PWD", "/server/working-directory");
        cmd.env("OLDPWD", "/server/previous-directory");

        let std_cmd = cmd.to_std_command().expect("build std command");
        let envs: BTreeMap<OsString, Option<OsString>> = std_cmd
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
            .collect();

        assert_eq!(
            envs.get(OsStr::new("PWD")),
            Some(&Some(scratch.path().as_os_str().to_owned()))
        );
        assert!(!envs.contains_key(OsStr::new("OLDPWD")));
    }

    #[test]
    fn login_shell_execs_shell_env_without_arguments() {
        let scratch = shepr_test_support::ScratchDir::new("pty-login-shell");
        let shell = fixture::stand_in(
            scratch.path(),
            "shepr-login-shell",
            &[fixture::Step::Sleep(std::time::Duration::from_secs(30))],
        );
        let shell = shell.to_str().expect("scratch shell path is UTF-8");
        let mut cmd = PtyCommand::interactive_shell(shell, true);
        cmd.cwd(scratch.path());
        let mut spawned =
            crate::backend::spawn_pty(shepr_core::geometry::PaneGeometry::new(80, 24, 0, 0), &cmd)
                .expect("spawn shell fixture");
        let command_line = std::fs::read(format!("/proc/{}/cmdline", spawned.child.id()));
        spawned
            .child
            .kill()
            .expect("stop the sleeping shell fixture");
        spawned.child.wait().expect("reap the shell fixture");
        let command_line = command_line.expect("read the fixture command line");
        let argv0_end = command_line
            .iter()
            .position(|byte| *byte == 0)
            .expect("command line has an argv0 terminator");
        assert_eq!(
            OsStr::from_bytes(&command_line[..argv0_end]),
            OsStr::new("-shepr-login-shell")
        );
        assert!(command_line[argv0_end + 1..].is_empty());
    }

    #[test]
    fn child_sees_resolved_shell_not_a_non_executable_shell_env() {
        let mut cmd = fixture_shell(&[fixture::Step::PrintEnv("SHELL".into())]);
        let selected = cmd
            .get_env(ChildEnv::Shell)
            .expect("selected shell")
            .to_owned();
        cmd.env("SHELL", "/__shepr_missing_shell__");
        let mut std_cmd = cmd.to_std_command().expect("build std command");
        std_cmd.stdout(std::process::Stdio::piped());
        let output = std_cmd.output().expect("run fixture child");

        assert!(output.status.success());
        let shell = output
            .stdout
            .strip_suffix(b"\n")
            .expect("fixture prints SHELL with a newline");
        let shell = OsStr::from_bytes(shell);
        assert_eq!(shell, selected.as_os_str());
        assert!(Path::new(shell).is_absolute());
        assert_eq!(
            classify_candidate(Path::new(shell)),
            CandidateStatus::Executable
        );
    }

    #[test]
    fn pane_shell_uses_its_resolved_program_without_environment_fallback() {
        let mut cmd = PtyCommand::interactive_shell("/__shepr_missing_shell__", false);
        cmd.env(ChildEnv::Shell, fixture::path_str());

        let err = cmd
            .to_std_command()
            .expect_err("the selected pane shell must fail instead of using inherited SHELL");

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn home_fallback_requires_an_existing_absolute_directory() {
        let scratch = shepr_test_support::ScratchDir::new("pty-home");
        let mut cmd = PtyCommand::interactive_shell(fixture::path_str(), false);
        cmd.env("HOME", scratch.path());
        assert_eq!(cmd.home_dir(), scratch.path().as_os_str());

        for unusable in [
            OsString::from("relative/home"),
            scratch.path().join("missing").into_os_string(),
        ] {
            cmd.env("HOME", &unusable);
            let home = cmd.home_dir();
            assert_ne!(home, unusable);
            assert!(Path::new(&home).is_absolute());
            assert!(is_directory(Path::new(&home)).expect("test precondition"));
        }
    }

    #[test]
    fn unusable_requested_cwd_starts_in_the_validated_home_fallback() {
        let scratch = shepr_test_support::ScratchDir::new("pty-stale-cwd");
        let mut cmd = PtyCommand::interactive_shell(fixture::path_str(), false);
        cmd.env(EnvVar::Home, scratch.path());
        cmd.cwd(scratch.join("removed-before-spawn"));

        let std_cmd = cmd
            .to_std_command()
            .expect("build command with cwd fallback");

        assert_eq!(std_cmd.get_current_dir(), Some(scratch.path()));
    }

    #[test]
    fn missing_configured_shell_is_reported_before_spawn() {
        let cmd = PtyCommand::interactive_shell("/__shepr_missing_program__", false);
        let err = cmd
            .to_std_command()
            .expect_err("missing program must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
