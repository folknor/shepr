//! Shepr-owned description of the shell to launch inside a pane PTY.
//!
//! `PtyCommand` starts from the server's own environment (see `base_env` for
//! why it is inherited whole) and lets callers set, remove, and inspect
//! variables before spawn, so pane launch policy (terminal identity, stripped
//! host and agent variables, integration variables) is plain data that tests
//! can assert on without spawning anything.

use std::collections::BTreeMap;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::limits::{
    PASSWD_BUFFER_GROWTH_FACTOR, PASSWD_BUFFER_INITIAL_BYTES, PASSWD_BUFFER_MAX_BYTES,
};

use shepr_core::env::{ChildEnv, EnvVar};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtyCommand {
    program: shepr_core::shell::ResolvedShell,
    login: bool,
    envs: BTreeMap<OsString, OsString>,
    cwd: Option<OsString>,
    cwd_required: bool,
}

/// Where the child may start, in order, with the environment for each: `PWD`
/// names the directory the child actually entered.
#[derive(Debug)]
pub(crate) struct LaunchCandidate {
    pub(crate) path: OsString,
    pub(crate) dir: CString,
    pub(crate) envp: Vec<CString>,
}

/// Every byte the forked child needs, built before the fork: the child may
/// not allocate.
#[derive(Debug)]
pub(crate) struct LaunchSpec {
    pub(crate) program: CString,
    pub(crate) argv: Vec<CString>,
    pub(crate) candidates: Vec<LaunchCandidate>,
}

impl PtyCommand {
    /// Run the shell selected and resolved while the server loaded its config.
    pub fn interactive_shell(
        default_shell: &shepr_core::shell::ResolvedShell,
        login: bool,
    ) -> Self {
        Self {
            program: default_shell.clone(),
            login,
            envs: base_env(),
            cwd: None,
            cwd_required: false,
        }
    }

    /// The executable path used for `execve`, independent of the child's `SHELL` value.
    pub fn program(&self) -> &OsStr {
        self.program.path().as_os_str()
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

    /// Working directory. Unless [`Self::require_cwd`] is set, a directory the
    /// child cannot enter falls back to `HOME`, then the passwd home, then
    /// `/`. The child decides, by chdir; nothing is checked before the fork.
    pub fn cwd<D: AsRef<OsStr>>(&mut self, dir: D) {
        self.cwd = Some(dir.as_ref().to_owned());
    }

    /// Start only in the requested directory: if the child cannot enter it,
    /// the launch fails instead of starting in `HOME`. Restored panes and
    /// agent resumes use this so they never run in a different directory.
    pub fn require_cwd(&mut self) {
        self.cwd_required = true;
    }

    /// The directories the child tries, in order, deduplicated.
    fn cwd_candidates(&self, passwd_home: Option<&OsStr>) -> io::Result<Vec<OsString>> {
        if self.cwd_required {
            let requested = self.cwd.clone().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "required pty working directory was not set",
                )
            })?;
            return Ok(vec![requested]);
        }
        let mut candidates: Vec<OsString> = Vec::new();
        let home = shepr_core::pathutil::home_dir_from_env_value(self.get_env(EnvVar::Home)).ok();
        let absolute = |path: &OsStr| Path::new(path).is_absolute();
        for candidate in [
            self.cwd.as_deref(),
            home.as_deref().map(Path::as_os_str),
            passwd_home.filter(|home| absolute(home)),
            Some(OsStr::new("/")),
        ]
        .into_iter()
        .flatten()
        {
            if !candidates.iter().any(|existing| existing == candidate) {
                candidates.push(candidate.to_owned());
            }
        }
        Ok(candidates)
    }

    /// The launch as the forked child runs it: the absolute shell path, its
    /// argv (a login shell gets `-name` as argv0 and no arguments), and for
    /// each cwd candidate exactly this command's environment with `PWD` set to
    /// that directory, `OLDPWD` dropped and `SHELL` the program itself.
    pub(crate) fn launch_spec(&self, passwd_home: Option<&OsStr>) -> io::Result<LaunchSpec> {
        let program_path = self.program.path();
        let program = c_string(program_path.as_os_str(), "pane shell path")?;
        let argv0 = if self.login {
            let basename = program_path.file_name().unwrap_or(program_path.as_os_str());
            let mut argv0 = OsString::from("-");
            argv0.push(basename);
            argv0
        } else {
            program_path.as_os_str().to_owned()
        };
        let argv = vec![c_string(&argv0, "pane shell argv0")?];
        let mut base = self.envs.clone();
        base.remove(OsStr::new("OLDPWD"));
        base.insert(
            OsString::from(ChildEnv::Shell.name()),
            program_path.as_os_str().to_owned(),
        );
        let candidates = self
            .cwd_candidates(passwd_home)?
            .into_iter()
            .map(|path| {
                let mut env = base.clone();
                // `PWD` belongs to the directory the child entered; a
                // relative one is left for the shell to reconstruct.
                if Path::new(&path).is_absolute() {
                    env.insert(OsString::from("PWD"), path.clone());
                } else {
                    env.remove(OsStr::new("PWD"));
                }
                let envp = env
                    .iter()
                    .map(|(key, value)| {
                        let mut entry = key.clone();
                        entry.push("=");
                        entry.push(value);
                        c_string(&entry, "pane environment entry")
                    })
                    .collect::<io::Result<Vec<_>>>()?;
                Ok(LaunchCandidate {
                    dir: c_string(&path, "pane working directory")?,
                    path,
                    envp,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(LaunchSpec {
            program,
            argv,
            candidates,
        })
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
/// decision per registered variable. Shell selection and validation happen once
/// when server config is loaded.
#[expect(
    clippy::disallowed_methods,
    reason = "a pane child inherits the server's environment verbatim; it is copied, not interpreted, and pane launch policy then edits the copy"
)]
fn base_env() -> BTreeMap<OsString, OsString> {
    std::env::vars_os().collect()
}

/// The current user's passwd home directory, read once per process.
pub(crate) fn passwd_home() -> Option<OsString> {
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
        if status != 0 || result.is_null() || entry.pw_dir.is_null() {
            return None;
        }
        // SAFETY: after a successful getpwuid_r call, pw_dir is a
        // NUL-terminated string inside the still-live result buffer.
        let bytes = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) }.to_bytes();
        return Some(OsStr::from_bytes(bytes).to_owned());
    }
}

/// A NUL-terminated copy of `value`, refusing an interior NUL.
pub(crate) fn c_string(value: &OsStr, what: &str) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{what} contains a NUL byte"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::fixture;
    use shepr_test_support::fixture::resolved_shell as test_shell;
    use std::os::unix::ffi::OsStrExt;

    fn env_of(candidate: &LaunchCandidate) -> BTreeMap<OsString, OsString> {
        candidate
            .envp
            .iter()
            .map(|entry| {
                let bytes = entry.as_bytes();
                let split = bytes
                    .iter()
                    .position(|byte| *byte == b'=')
                    .expect("environment entry has a separator");
                (
                    OsStr::from_bytes(&bytes[..split]).to_owned(),
                    OsStr::from_bytes(&bytes[split + 1..]).to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn padded_or_relative_home_is_not_a_cwd_fallback() {
        let _env = shepr_test_support::IsolatedEnv::new();
        for home in ["/home/pane ", "relative", ""] {
            let mut command =
                PtyCommand::interactive_shell(&test_shell(fixture::path_str()), false);
            command.env(EnvVar::Home, home);
            assert_eq!(
                command.cwd_candidates(None).expect("candidates"),
                vec![OsString::from("/")]
            );
        }
    }

    #[test]
    fn env_edits_are_visible_before_spawn() {
        let mut cmd = PtyCommand::interactive_shell(&test_shell(fixture::path_str()), false);
        cmd.env("SHEPR_PTY_TEST_KEY", "value");
        assert_eq!(cmd.get_env("SHEPR_PTY_TEST_KEY"), Some(OsStr::new("value")));
        cmd.env_remove("SHEPR_PTY_TEST_KEY");
        assert!(cmd.get_env("SHEPR_PTY_TEST_KEY").is_none());
        cmd.env_remove("SHELL");
        assert!(cmd.get_env("SHELL").is_none());
    }

    #[test]
    fn the_launch_carries_exactly_the_command_env() {
        let mut cmd = PtyCommand::interactive_shell(&test_shell(fixture::path_str()), false);
        cmd.env("SHEPR_PTY_TEST_SET", "1");
        cmd.env("SHEPR_PTY_TEST_REMOVED", "1");
        cmd.env_remove("SHEPR_PTY_TEST_REMOVED");
        let spec = cmd.launch_spec(None).expect("build launch");
        for candidate in &spec.candidates {
            let env = env_of(candidate);
            assert_eq!(
                env.get(OsStr::new("SHEPR_PTY_TEST_SET")),
                Some(&OsString::from("1"))
            );
            assert!(!env.contains_key(OsStr::new("SHEPR_PTY_TEST_REMOVED")));
        }
    }

    #[test]
    fn each_candidate_sets_pwd_to_its_directory_and_drops_server_oldpwd() {
        let scratch = shepr_test_support::ScratchDir::new("pty-command-cwd-env");
        let mut cmd = PtyCommand::interactive_shell(&test_shell(fixture::path_str()), false);
        cmd.cwd(scratch.path());
        cmd.env("HOME", "/");
        cmd.env("PWD", "/server/working-directory");
        cmd.env("OLDPWD", "/server/previous-directory");

        let spec = cmd.launch_spec(None).expect("build launch");
        assert_eq!(spec.candidates[0].path, scratch.path().as_os_str());
        for candidate in &spec.candidates {
            let env = env_of(candidate);
            assert_eq!(env.get(OsStr::new("PWD")), Some(&candidate.path));
            assert!(!env.contains_key(OsStr::new("OLDPWD")));
        }
    }

    #[test]
    fn fallback_candidates_are_home_then_passwd_home_then_root_without_repeats() {
        let mut cmd = PtyCommand::interactive_shell(&test_shell(fixture::path_str()), false);
        cmd.cwd("/requested");
        cmd.env("HOME", "/home/user");
        let spec = cmd
            .launch_spec(Some(OsStr::new("/home/user")))
            .expect("build launch");
        let paths: Vec<_> = spec.candidates.iter().map(|c| c.path.clone()).collect();
        assert_eq!(paths, ["/requested", "/home/user", "/"]);

        cmd.env("HOME", "relative/home");
        let spec = cmd
            .launch_spec(Some(OsStr::new("/passwd/home")))
            .expect("build launch");
        let paths: Vec<_> = spec.candidates.iter().map(|c| c.path.clone()).collect();
        assert_eq!(paths, ["/requested", "/passwd/home", "/"]);
    }

    #[test]
    fn a_required_cwd_has_no_fallback() {
        let mut cmd = PtyCommand::interactive_shell(&test_shell(fixture::path_str()), false);
        cmd.cwd("/requested");
        cmd.require_cwd();
        let spec = cmd
            .launch_spec(Some(OsStr::new("/home")))
            .expect("build launch");
        assert_eq!(spec.candidates.len(), 1);
        assert_eq!(spec.candidates[0].path, "/requested");
    }

    #[test]
    fn the_child_sees_the_selected_shell_not_an_inherited_shell_env() {
        let mut cmd = PtyCommand::interactive_shell(&test_shell(fixture::path_str()), false);
        cmd.env("SHELL", "/__shepr_missing_shell__");
        let spec = cmd.launch_spec(None).expect("build launch");
        for candidate in &spec.candidates {
            assert_eq!(
                env_of(candidate).get(OsStr::new("SHELL")),
                Some(&OsString::from(fixture::path_str()))
            );
        }
    }

    #[test]
    fn a_login_shell_gets_a_dash_argv0_and_no_arguments() {
        // host-program-ok: the path is only turned into an argv, never run.
        let cmd = PtyCommand::interactive_shell(&test_shell("/bin/zsh"), true);
        let spec = cmd.launch_spec(None).expect("build launch");
        assert_eq!(spec.argv.len(), 1);
        assert_eq!(spec.argv[0].as_bytes(), b"-zsh");
    }
}
