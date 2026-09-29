//! The one way shepr runs Git: a short read-only probe with a deadline, no
//! terminal, no prompts and no repository selection inherited from the
//! caller's environment. The server's Git status and the client's workspace
//! label both go through [`run_git`]; what the output means stays with them.

use std::ffi::OsStr;
use std::io::{self, Read};
use std::path::Path;
use std::process::{ChildStderr, ChildStdout, Output, Stdio};
use std::time::{Duration, Instant};

/// How long one Git probe may run before it is killed.
pub const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a Git probe produced no output to interpret.
#[derive(Debug)]
pub enum GitCommandError {
    /// The Git executable could not be started.
    Spawn(io::Error),
    /// Git did not finish before the deadline and was killed.
    TimedOut,
    /// The child could not be waited for, or its output could not be read.
    Process(io::Error),
}

impl std::fmt::Display for GitCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "could not start git: {error}"),
            Self::TimedOut => write!(formatter, "git timed out"),
            Self::Process(error) => write!(formatter, "git process failed: {error}"),
        }
    }
}

impl std::error::Error for GitCommandError {}

/// Runs `git -C <cwd> <args>` under [`GIT_COMMAND_TIMEOUT`]. A nonzero exit is
/// an `Ok` output; the caller decides which failures are ordinary answers.
pub fn run_git(cwd: &Path, args: &[&str]) -> Result<Output, GitCommandError> {
    run_git_with_program(OsStr::new("git"), cwd, args, GIT_COMMAND_TIMEOUT)
}

/// [`run_git`] with the program and deadline handed in, for tests that stand
/// a fixture in for Git.
pub fn run_git_with_program(
    program: &OsStr,
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Output, GitCommandError> {
    // host-program-ok: production asks Git about the repository it inspects
    let mut command = crate::child_command(program, cwd);
    command
        .arg("-C")
        .arg(cwd)
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        // Repository selectors from the caller's environment would point the
        // probe at some other repository than `cwd`'s; prompt helpers could
        // open a dialog. Config-source overrides and GIT_CEILING_DIRECTORIES
        // stay, so Git follows the user's own configuration and discovery.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_PREFIX")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("SSH_ASKPASS_REQUIRE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(GitCommandError::Spawn)?;
    // Both pipes are drained while the child runs, so output larger than a
    // pipe buffer cannot stall Git into a spurious timeout.
    let stdout = child.stdout.take().map(drain_pipe::<ChildStdout>);
    let stderr = child.stderr.take().map(drain_pipe::<ChildStderr>);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(super::limits::HELPER_PROCESS_POLL_INTERVAL);
            }
            Ok(None) => {
                kill_and_reap(&mut child);
                // The readers are not joined: whatever still holds the pipes
                // open must not hold this probe past its deadline too.
                return Err(GitCommandError::TimedOut);
            }
            Err(error) => {
                kill_and_reap(&mut child);
                return Err(GitCommandError::Process(error));
            }
        }
    };
    Ok(Output {
        status,
        stdout: join_drain(stdout)?,
        stderr: join_drain(stderr)?,
    })
}

type Drain = Result<std::thread::JoinHandle<io::Result<Vec<u8>>>, io::Error>;

fn drain_pipe<R: Read + Send + 'static>(mut pipe: R) -> Drain {
    std::thread::Builder::new()
        .name("shepr-git-pipe".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).map(|_| bytes)
        })
}

fn join_drain(drain: Option<Drain>) -> Result<Vec<u8>, GitCommandError> {
    let Some(drain) = drain else {
        return Ok(Vec::new());
    };
    let handle = drain.map_err(GitCommandError::Process)?;
    match handle.join() {
        Ok(read) => read.map_err(GitCommandError::Process),
        Err(_) => Err(GitCommandError::Process(io::Error::other(
            "git output reader panicked",
        ))),
    }
}

fn kill_and_reap(child: &mut std::process::Child) {
    if let Err(error) = child.kill()
        && error.kind() != io::ErrorKind::InvalidInput
    {
        tracing::debug!(%error, "failed to stop a git probe");
    }
    if let Err(error) = child.wait() {
        tracing::debug!(%error, "failed to reap a git probe");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::fixture::{Step, stand_in};

    #[test]
    fn git_runner_sanitizes_terminal_inputs_and_enforces_a_deadline() {
        let env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("git-runner");
        // The stand-in prints the variables the runner must scrub or set, then
        // copies stdin: a null stdin ends at once, an inherited one would hang
        // past the deadline.
        let fake_git = stand_in(
            root.path(),
            "fake-git",
            &[
                Step::PrintEnv("GIT_DIR".into()),
                Step::PrintEnv("GIT_WORK_TREE".into()),
                Step::PrintEnv("GIT_ASKPASS".into()),
                Step::PrintEnv("GIT_TERMINAL_PROMPT".into()),
                Step::PrintEnv("GIT_OPTIONAL_LOCKS".into()),
                Step::Cat,
            ],
        );
        env.set("GIT_DIR", "/unrelated/repository");
        env.set("GIT_WORK_TREE", "/unrelated/worktree");
        env.set("GIT_ASKPASS", "/unrelated/askpass");

        let output = run_git_with_program(
            fake_git.as_os_str(),
            root.path(),
            &[],
            Duration::from_secs(5),
        )
        .expect("fake git should finish");
        assert_eq!(output.stdout, b"\n\n\n0\n0\n");

        let slow_git = stand_in(
            root.path(),
            "slow-git",
            &[Step::Sleep(Duration::from_secs(30))],
        );
        let result = run_git_with_program(
            slow_git.as_os_str(),
            root.path(),
            &[],
            Duration::from_millis(20),
        );
        assert!(matches!(result, Err(GitCommandError::TimedOut)));
    }

    /// Output larger than a pipe buffer is drained while Git runs rather
    /// than stalling it into a timeout.
    #[test]
    fn git_runner_drains_output_larger_than_a_pipe_buffer() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("git-runner-large");
        let fake_git = stand_in(
            root.path(),
            "fake-git",
            &[Step::Fill {
                byte: b'x',
                count: 1024 * 1024,
            }],
        );
        let output = run_git_with_program(
            fake_git.as_os_str(),
            root.path(),
            &[],
            Duration::from_secs(5),
        )
        .expect("fake git should finish");
        assert_eq!(output.stdout.len(), 1024 * 1024);
    }
}
