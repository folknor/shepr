//! The one way shepr runs Git: a short read-only probe with a deadline, no
//! terminal, no prompts and no repository selection inherited from the
//! caller's environment. Status, discovery and every other Git probe go
//! through [`run_git`]; what the output means stays with each caller.
//!
//! The child itself is supervised by `shepr_platform::supervised`: its
//! deadline, output caps, process-group kill and reaping, under Git's own
//! child budget so a hung mount's stuck probes cannot take another
//! subsystem's capacity.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use shepr_platform::supervised::{
    ChildBudget, ChildEnd, ChildStream, Overflow, PreparedChild, StreamSpec, SupervisedRun,
    run_supervised,
};

use crate::limits::{GIT_CHILD_BUDGET, GIT_COMMAND_TIMEOUT, MAX_GIT_PIPE_BYTES};

/// Git probes running or waiting to be reaped. A probe stuck on a hung mount
/// keeps its slot until the kernel releases it.
static GIT_CHILDREN: ChildBudget = ChildBudget::new("git", GIT_CHILD_BUDGET);

/// Why a Git probe produced no output to interpret.
#[derive(Debug)]
pub enum GitCommandError {
    /// The Git executable could not be started, or Git's child budget is
    /// spent on probes that are still stuck.
    Spawn(io::Error),
    /// The Git probe did not finish before the deadline.
    TimedOut,
    /// The child could not be supervised, or its output was unusable.
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
/// The call returns within the budget plus the supervisor's kill grace; a
/// child that outlives its SIGKILL is reaped in the background. A nonzero
/// exit is an `Ok` output; the caller decides which failures are ordinary
/// answers. Output past the per-stream cap fails the probe rather than
/// returning a prefix that could hide config dependencies or refs.
pub fn run_git(cwd: &Path, args: &[&str]) -> Result<Output, GitCommandError> {
    crate::access::check_command().map_err(GitCommandError::Spawn)?;
    let program = crate::access::git_program().map_err(GitCommandError::Spawn)?;
    run_git_with_program(&program, cwd, args, GIT_COMMAND_TIMEOUT)
}

/// [`run_git`] with the program and deadline handed in, for tests that stand
/// a fixture in for Git.
fn run_git_with_program(
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
    let deadline = now() + timeout;
    crate::access::check_command().map_err(GitCommandError::Spawn)?;
    let mut command = git_command(program, cwd).map_err(GitCommandError::Spawn)?;
    command
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        // Repository selectors from the caller's environment would point the
        // probe at some other repository than `cwd`'s; prompt helpers could
        // open a dialog. Config-source overrides, GIT_CEILING_DIRECTORIES and
        // GIT_DISCOVERY_ACROSS_FILESYSTEM stay, so Git follows the user's own
        // configuration and discovery.
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
        .env_remove(shepr_core::env::ChildEnv::SshAskpass)
        .env_remove(shepr_core::env::ChildEnv::SshAskpassRequire);
    let capture = |stream| StreamSpec {
        stream,
        cap: MAX_GIT_PIPE_BYTES,
        overflow: Overflow::Terminate,
    };
    let report = run_supervised(
        SupervisedRun {
            budget: &GIT_CHILDREN,
            extra_pipes: 0,
            streams: vec![capture(ChildStream::Stdout), capture(ChildStream::Stderr)],
            deadline,
        },
        |_| PreparedChild {
            command,
            stdin: None,
        },
    );
    let status = match report.end {
        ChildEnd::Exited(status) => status,
        ChildEnd::TimedOut => return Err(GitCommandError::TimedOut),
        ChildEnd::Overflowed => {
            return Err(GitCommandError::Process(io::Error::new(
                io::ErrorKind::InvalidData,
                "git output exceeds the per-stream byte limit",
            )));
        }
        ChildEnd::Exhausted => {
            return Err(GitCommandError::Spawn(io::Error::new(
                io::ErrorKind::WouldBlock,
                "every git probe slot is held by a probe that is still stuck",
            )));
        }
        ChildEnd::Spawn(error) => return Err(GitCommandError::Spawn(error)),
        ChildEnd::Supervision(error) => return Err(GitCommandError::Process(error)),
    };
    let mut streams = report.streams.into_iter();
    let stdout = complete_stream(streams.next())?;
    let stderr = complete_stream(streams.next())?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// A stream's bytes, only when it was read to its end: a read failure is a
/// failed probe, never a prefix to interpret.
fn complete_stream(
    report: Option<shepr_platform::supervised::StreamReport>,
) -> Result<Vec<u8>, GitCommandError> {
    let report = report.unwrap_or_default();
    if let Some(kind) = report.error {
        return Err(GitCommandError::Process(io::Error::from(kind)));
    }
    if report.overflowed || !report.eof {
        return Err(GitCommandError::Process(io::Error::new(
            io::ErrorKind::InvalidData,
            "git output was not read to its end",
        )));
    }
    Ok(report.bytes)
}

/// Exec from the local root, then let Git change directory. A pre-exec chdir
/// into a hung workspace would retain every inherited server fd (including
/// the data-directory flock and PTY masters) until that mount recovers.
/// The close-on-exec flag closes those descriptors before Git evaluates `-C`, and the
/// resulting hang is covered by the ordinary child deadline.
fn git_command(program: &OsStr, cwd: &Path) -> io::Result<Command> {
    let cwd = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir()?.join(cwd)
    };
    // host-program-ok: production asks Git about the repository it inspects
    let mut command = shepr_platform::child_command(program, Path::new("/"));
    command.arg("-C").arg(cwd);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::fixture::{Step, stand_in};

    #[test]
    fn git_changes_directory_only_after_exec() {
        use std::os::unix::ffi::OsStrExt;
        let cwd = Path::new(OsStr::from_bytes(b"/unavailable/worktree-\xff"));
        let command = git_command(OsStr::new("git"), cwd).expect("command");
        assert_eq!(command.get_current_dir(), Some(Path::new("/")));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [OsStr::new("-C"), cwd.as_os_str()]
        );
    }

    #[test]
    fn git_spawn_does_not_touch_the_workspace_directory() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("git-spawn-cwd");
        let fake_git = stand_in(root.path(), "fake-git", &[Step::Cat]);
        // The fixture ignores -C. Spawn must succeed even when the workspace
        // cannot be entered; real Git reports that failure after exec.
        let output = run_git_with_program(
            fake_git.as_os_str(),
            &root.path().join("missing-workspace"),
            &[],
            Duration::from_secs(5),
        )
        .expect("spawn does not chdir into the workspace");
        assert!(output.status.success());
    }

    #[test]
    fn git_runner_refuses_oversized_output_instead_of_returning_a_prefix() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("git-runner-output-limit");
        let fake_git = stand_in(
            root.path(),
            "fake-git",
            &[Step::Fill {
                byte: b'x',
                count: MAX_GIT_PIPE_BYTES + 1,
            }],
        );
        let result = run_git_with_program(
            fake_git.as_os_str(),
            root.path(),
            &[],
            Duration::from_secs(30),
        );
        assert!(matches!(
            result,
            Err(GitCommandError::Process(error)) if error.kind() == io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn git_runner_accepts_output_at_the_limit() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("git-runner-output-boundary");
        let fake_git = stand_in(
            root.path(),
            "fake-git",
            &[Step::Fill {
                byte: b'x',
                count: MAX_GIT_PIPE_BYTES,
            }],
        );
        let output = run_git_with_program(
            fake_git.as_os_str(),
            root.path(),
            &[],
            Duration::from_secs(30),
        )
        .expect("output at the limit is complete");
        assert_eq!(output.stdout.len(), MAX_GIT_PIPE_BYTES);
    }

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
        let result = run_git_with_program(
            slow_git.as_os_str(),
            root.path(),
            &[],
            Duration::from_millis(200),
        );
        assert!(matches!(result, Err(GitCommandError::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(10));
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

    /// The child starts in `/` and names `cwd` once, as an absolute `-C`
    /// operand ahead of every other option, so Git resolves it exactly once.
    #[test]
    fn git_runner_names_cwd_once_as_the_leading_directory_option() {
        use std::os::unix::ffi::OsStrExt;
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

        let mut expected = b"-C\n".to_vec();
        expected.extend_from_slice(root.path().as_os_str().as_bytes());
        expected.extend_from_slice(b"\n-c\ncore.fsmonitor=false\nrev-parse\n--show-prefix\n");
        assert_eq!(output.stdout, expected);
    }
}
