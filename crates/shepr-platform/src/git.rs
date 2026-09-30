//! The one way shepr runs Git: a short read-only probe with a deadline, no
//! terminal, no prompts and no repository selection inherited from the
//! caller's environment. The server's Git status and workspace checkout
//! probes go through [`run_git`]; what the output means stays with them.

use std::ffi::OsStr;
use std::io::{self, Read};
use std::path::Path;
use std::process::{ChildStderr, ChildStdout, Output, Stdio};
use std::time::{Duration, Instant};

pub use super::limits::GIT_COMMAND_TIMEOUT;

/// Why a Git probe produced no output to interpret.
#[derive(Debug)]
pub enum GitCommandError {
    /// The Git executable could not be started.
    Spawn(io::Error),
    /// The Git probe did not finish before the deadline.
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

/// Runs Git in `cwd` with one budget for launch, execution and pipe draining.
/// The synchronous OS spawn cannot be interrupted; if it exceeds
/// [`GIT_COMMAND_TIMEOUT`], the child is stopped as soon as spawn returns.
/// A nonzero exit is an `Ok` output; the caller decides which failures are
/// ordinary answers.
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
    // clock-io-ok: the public entry point supplies the real clock.
    run_git_with_program_and_clock(program, cwd, args, timeout, &Instant::now)
}

fn run_git_with_program_and_clock(
    program: &OsStr,
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
    now: &dyn Fn() -> Instant,
) -> Result<Output, GitCommandError> {
    // host-program-ok: production asks Git about the repository it inspects
    let mut command = crate::child_command(program, cwd);
    command
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        // Repository selectors from the caller's environment would point the
        // probe at some other repository than `cwd`'s; prompt helpers could
        // open a dialog. Config-source overrides and GIT_CEILING_DIRECTORIES
        // stay, so Git follows the user's own configuration and discovery.
        // GIT_CONFIG selects a file only for `git config`, not for Git's
        // ordinary repository commands. Keep origin probes on the same
        // effective chain as the ref queries they cache.
        .env_remove("GIT_CONFIG")
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
    let deadline = now() + timeout;
    let mut child = command.spawn().map_err(GitCommandError::Spawn)?;
    // Both pipes are drained while the child runs, so output larger than a
    // pipe buffer cannot stall Git into a spurious timeout.
    let stdout = child.stdout.take().map(drain_pipe::<ChildStdout>);
    let stderr = child.stderr.take().map(drain_pipe::<ChildStderr>);
    let status = loop {
        if now() >= deadline {
            kill_and_reap(&mut child);
            return Err(GitCommandError::TimedOut);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                std::thread::sleep(super::limits::HELPER_PROCESS_POLL_INTERVAL);
            }
            Err(error) => {
                kill_and_reap(&mut child);
                return Err(GitCommandError::Process(error));
            }
        }
    };
    let (stdout, stderr) = join_drains_until(stdout, stderr, deadline, now)?;
    Ok(Output {
        status,
        stdout,
        stderr,
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

/// Waits for both pipe readers without extending the child deadline. A child
/// can exit while a descendant still holds one of its inherited pipe ends.
fn join_drains_until(
    stdout: Option<Drain>,
    stderr: Option<Drain>,
    deadline: Instant,
    now: &dyn Fn() -> Instant,
) -> Result<(Vec<u8>, Vec<u8>), GitCommandError> {
    loop {
        if drain_finished(&stdout) && drain_finished(&stderr) {
            break;
        }
        if now() >= deadline {
            return Err(GitCommandError::TimedOut);
        }
        std::thread::sleep(super::limits::HELPER_PROCESS_POLL_INTERVAL);
    }
    Ok((join_drain(stdout)?, join_drain(stderr)?))
}

fn drain_finished(drain: &Option<Drain>) -> bool {
    match drain {
        Some(Ok(handle)) => handle.is_finished(),
        None | Some(Err(_)) => true,
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
    fn git_runner_samples_deadline_even_when_spawn_fails() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("git-spawn-clock");
        let reads = std::cell::Cell::new(0);
        let now = || {
            reads.set(reads.get() + 1);
            Instant::now()
        };
        let missing = root.path().join("missing-program");
        let result = run_git_with_program_and_clock(
            missing.as_os_str(),
            root.path(),
            &[],
            Duration::from_secs(1),
            &now,
        );
        assert!(matches!(result, Err(GitCommandError::Spawn(_))));
        assert_eq!(reads.get(), 1, "the budget starts before attempting spawn");
    }

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
                Step::PrintEnv("GIT_CONFIG".into()),
                Step::PrintEnv("GIT_DIR".into()),
                Step::PrintEnv("GIT_WORK_TREE".into()),
                Step::PrintEnv("GIT_ASKPASS".into()),
                Step::PrintEnv("GIT_TERMINAL_PROMPT".into()),
                Step::PrintEnv("GIT_OPTIONAL_LOCKS".into()),
                Step::Cat,
            ],
        );
        env.set("GIT_CONFIG", "/unrelated/config");
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
        assert_eq!(output.stdout, b"\n\n\n\n0\n0\n");

        let slow_git = stand_in(
            root.path(),
            "slow-git",
            &[Step::Sleep(Duration::from_secs(30))],
        );
        let started = Instant::now();
        let deadline = started + Duration::from_millis(20);
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reads_for_clock = std::sync::Arc::clone(&reads);
        let now = move || {
            if reads_for_clock.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                started
            } else {
                deadline
            }
        };
        let result = run_git_with_program_and_clock(
            slow_git.as_os_str(),
            root.path(),
            &[],
            Duration::from_millis(20),
            &now,
        );
        assert!(matches!(result, Err(GitCommandError::TimedOut)));
        assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 2);
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

    #[test]
    fn git_runner_deadline_also_covers_output_drain_after_child_exit() {
        let (release, wait) = std::sync::mpsc::channel();
        let stdout: Drain = Ok(std::thread::spawn(move || {
            // A release or a dropped sender both end the wait.
            let _released = wait.recv();
            Ok(Vec::new())
        }));
        let deadline = Instant::now() + Duration::from_millis(30);
        let started = Instant::now();
        let result = join_drains_until(Some(stdout), None, deadline, &Instant::now);
        let elapsed = started.elapsed();
        release.send(()).expect("release the detached reader");

        assert!(matches!(result, Err(GitCommandError::TimedOut)));
        assert!(
            elapsed < Duration::from_secs(1),
            "pipe drain exceeded its deadline: {elapsed:?}"
        );
    }

    /// The child already starts in `cwd`. Passing it again as `-C <cwd>`
    /// would resolve a relative `cwd` a second time, from inside itself.
    #[test]
    fn git_runner_sets_cwd_once_and_passes_no_directory_option() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("git-runner-cwd");
        let fake_git = stand_in(root.path(), "fake-git", &[Step::PrintArgs]);
        let output = run_git_with_program(
            fake_git.as_os_str(),
            root.path(),
            &["rev-parse", "--show-prefix"],
            Duration::from_secs(5),
        )
        .expect("fake git should finish");

        assert_eq!(
            output.stdout,
            b"-c\ncore.fsmonitor=false\nrev-parse\n--show-prefix\n"
        );
    }
}
