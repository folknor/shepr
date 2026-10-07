//! The one way shepr runs Git: a short read-only probe with a deadline, no
//! terminal, no prompts and no repository selection inherited from the
//! caller's environment. Status, discovery and every other Git probe go
//! through [`run_git`]; what the output means stays with each caller.

use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::limits::{
    GIT_COMMAND_TIMEOUT, GIT_KILL_REAP_GRACE, GIT_PROCESS_POLL_INTERVAL, MAX_UNREAPED_GIT_CHILDREN,
    UNREAPED_GIT_CHILD_POLL_INTERVAL,
};

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
/// `GIT_COMMAND_TIMEOUT`, the child is stopped as soon as spawn returns.
/// Past that spawn, the call returns within the budget plus
/// `GIT_KILL_REAP_GRACE` and a few poll intervals: a child that outlives its
/// SIGKILL (stuck in an uninterruptible wait) is handed to a background
/// reaper rather than waited for, and the pipe reader threads stop reading at
/// the budget's deadline themselves, so no thread outlives it by more than one
/// poll. A killed child still stuck is a process that lives on until the kernel
/// releases it; only its reaping is deferred.
/// A nonzero exit is an `Ok` output; the caller decides which failures are
/// ordinary answers.
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
        .env_remove(shepr_core::env::ChildEnv::SshAskpass)
        .env_remove(shepr_core::env::ChildEnv::SshAskpassRequire)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let deadline = now() + timeout;
    let mut child = command.spawn().map_err(GitCommandError::Spawn)?;
    // Both pipes are drained while the child runs, so output larger than a
    // pipe buffer cannot stall Git into a spurious timeout.
    let stdout = child.stdout.take().map(|pipe| drain_pipe(pipe, deadline));
    let stderr = child.stderr.take().map(|pipe| drain_pipe(pipe, deadline));
    let status = loop {
        if now() >= deadline {
            kill_and_reap(child, GIT_KILL_REAP_GRACE);
            return Err(GitCommandError::TimedOut);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                std::thread::sleep(GIT_PROCESS_POLL_INTERVAL);
            }
            Err(error) => {
                kill_and_reap(child, GIT_KILL_REAP_GRACE);
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

/// Reads `pipe` to its end on its own thread, or until `deadline` passes,
/// which fails the read with [`io::ErrorKind::TimedOut`]. The thread waits for
/// readiness with a timeout, so a descendant that keeps the pipe's write end
/// open after Git is gone cannot hold it past the deadline. Output beyond the
/// per-stream byte budget fails with [`io::ErrorKind::InvalidData`].
fn drain_pipe<R: Read + AsRawFd + Send + 'static>(mut pipe: R, deadline: Instant) -> Drain {
    std::thread::Builder::new()
        .name("shepr-git-pipe".into())
        .spawn(move || read_until(&mut pipe, deadline))
}

fn read_until(pipe: &mut (impl Read + AsRawFd), deadline: Instant) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; crate::limits::GIT_PIPE_READ_CHUNK_BYTES];
    loop {
        // clock-io-ok: the reader thread runs against the real clock.
        let Some(remaining) = shepr_platform::remaining_until(deadline, Instant::now()) else {
            return Err(io::ErrorKind::TimedOut.into());
        };
        match shepr_platform::poll_fd_readable(pipe.as_raw_fd(), remaining) {
            Ok(true) => {}
            Ok(false) => return Err(io::ErrorKind::TimedOut.into()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        // Readable, or hung up: the read returns data or the end at once.
        match pipe.read(&mut chunk) {
            Ok(0) => return Ok(bytes),
            Ok(count) => {
                if count > crate::limits::MAX_GIT_PIPE_BYTES - bytes.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "git output exceeds the per-stream byte limit",
                    ));
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn join_drain(drain: Option<Drain>) -> Result<Vec<u8>, GitCommandError> {
    let Some(drain) = drain else {
        return Ok(Vec::new());
    };
    let handle = drain.map_err(GitCommandError::Process)?;
    match handle.join() {
        Ok(Err(error)) if error.kind() == io::ErrorKind::TimedOut => Err(GitCommandError::TimedOut),
        Ok(read) => read.map_err(GitCommandError::Process),
        Err(_) => Err(GitCommandError::Process(io::Error::other(
            "git output reader panicked",
        ))),
    }
}

/// Waits for both pipe readers without extending the child deadline. A child
/// can exit while a descendant still holds one of its inherited pipe ends; the
/// readers give up at that same deadline on their own, so a reader this
/// returns without joining is already on its way out.
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
        std::thread::sleep(GIT_PROCESS_POLL_INTERVAL);
    }
    Ok((join_drain(stdout)?, join_drain(stderr)?))
}

fn drain_finished(drain: &Option<Drain>) -> bool {
    match drain {
        Some(Ok(handle)) => handle.is_finished(),
        None | Some(Err(_)) => true,
    }
}

/// Kills `child` and waits at most `grace` for it to be reaped. A child that
/// is still alive after that (SIGKILL does not act on a process in an
/// uninterruptible wait until the kernel call returns) goes to the background
/// reaper, so the caller is never held by it.
fn kill_and_reap(mut child: Child, grace: Duration) {
    if let Err(error) = child.kill()
        && error.kind() != io::ErrorKind::InvalidInput
    {
        tracing::debug!(%error, "failed to stop a git probe");
    }
    // clock-io-ok: the grace is a real-time bound on the caller.
    let give_up = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(%error, "failed to reap a git probe");
                return;
            }
        }
        // clock-io-ok: as above.
        if Instant::now() >= give_up {
            break;
        }
        std::thread::sleep(GIT_PROCESS_POLL_INTERVAL);
    }
    hand_to_reaper(child);
}

/// Killed children that outlived their grace, and whether the thread that
/// reaps them is running. One thread polls them all and exits when none is
/// left, so a hung mount costs one thread however many probes it stalls.
struct Reaper {
    children: Vec<Child>,
    running: bool,
}

static REAPER: Mutex<Reaper> = Mutex::new(Reaper {
    children: Vec::new(),
    running: false,
});

fn lock_reaper() -> MutexGuard<'static, Reaper> {
    // Every critical section is a push, a retain or a flag, so a panic on the
    // holder leaves the value whole.
    REAPER.lock().unwrap_or_else(PoisonError::into_inner)
}

fn hand_to_reaper(child: Child) {
    let mut reaper = lock_reaper();
    if reaper.children.len() >= MAX_UNREAPED_GIT_CHILDREN {
        shepr_platform::structured_log!(
            WARN,
            event = git.probe_reap,
            outcome = Exhausted,
            pid = child.id(),
            "too many killed git probes are still unreaped; leaving this one a zombie until \
             shepr exits"
        );
        return;
    }
    tracing::debug!(pid = child.id(), "handed a killed git probe to the reaper");
    reaper.children.push(child);
    if reaper.running {
        return;
    }
    reaper.running = true;
    if let Err(error) = std::thread::Builder::new()
        // Linux exposes at most 15 bytes through `pthread_setname_np`.
        .name("git-reaper".into())
        .spawn(reap_until_empty)
    {
        // The children stay queued; the next hand-over starts the thread again.
        reaper.running = false;
        tracing::debug!(%error, "could not start the git probe reaper");
    }
}

fn reap_until_empty() {
    loop {
        std::thread::sleep(UNREAPED_GIT_CHILD_POLL_INTERVAL);
        let mut reaper = lock_reaper();
        // A child that cannot be waited for any more has nothing left to reap.
        reaper
            .children
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
        if reaper.children.is_empty() {
            reaper.running = false;
            return;
        }
    }
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
    fn pipe_output_limit_accepts_the_boundary_and_refuses_overflow() {
        for extra in [0, 1] {
            let (mut reader, mut writer) =
                std::os::unix::net::UnixStream::pair().expect("socket pair");
            let writing = std::thread::spawn(move || {
                use std::io::Write;
                let chunk = [b'x'; crate::limits::GIT_PIPE_READ_CHUNK_BYTES];
                for _ in 0..crate::limits::MAX_GIT_PIPE_BYTES / chunk.len() {
                    writer.write_all(&chunk).expect("write up to limit");
                }
                if extra > 0 {
                    writer.write_all(b"x").expect("write overflow byte");
                }
            });
            let result = read_until(&mut reader, Instant::now() + Duration::from_secs(30));
            writing.join().expect("writer finished");
            if extra == 0 {
                assert_eq!(
                    result.expect("output at limit").len(),
                    crate::limits::MAX_GIT_PIPE_BYTES
                );
            } else {
                assert_eq!(
                    result.expect_err("oversized output").kind(),
                    io::ErrorKind::InvalidData
                );
            }
        }
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
                count: crate::limits::MAX_GIT_PIPE_BYTES + 1,
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

    /// A descendant that keeps the pipe's write end open must not keep the
    /// reader thread alive past the deadline.
    #[test]
    fn pipe_reader_gives_up_at_its_deadline_while_the_write_end_stays_open() {
        let (reader, _writer) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        let started = Instant::now();
        let drain = drain_pipe(reader, started + Duration::from_millis(50)).expect("reader starts");
        let finished = loop {
            if drain.is_finished() {
                break true;
            }
            if started.elapsed() > Duration::from_secs(10) {
                break false;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(finished, "the reader outlived its deadline");
        let error = drain
            .join()
            .expect("reader did not panic")
            .expect_err("the reader timed out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn pipe_reader_returns_what_was_written_once_the_write_end_closes() {
        use std::io::Write;
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        let drain =
            drain_pipe(reader, Instant::now() + Duration::from_secs(30)).expect("reader starts");
        writer.write_all(b"status").expect("write");
        drop(writer);
        let bytes = drain
            .join()
            .expect("reader did not panic")
            .expect("reader finished");
        assert_eq!(bytes, b"status");
    }

    /// A killed child that is not reaped within the grace is handed to the
    /// background reaper: the caller returns at once and the child is still
    /// reaped later (a zombie stays in /proc until it is).
    #[test]
    fn a_killed_child_not_reaped_in_its_grace_is_reaped_in_the_background() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let root = shepr_test_support::ScratchDir::new("git-runner-reaper");
        let slow = stand_in(
            root.path(),
            "slow-git",
            &[Step::Sleep(Duration::from_secs(30))],
        );
        let child = shepr_platform::child_command(slow.as_os_str(), root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("stand-in starts");
        let proc_entry = std::path::PathBuf::from(format!("/proc/{}", child.id()));

        let started = Instant::now();
        kill_and_reap(child, Duration::ZERO);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the caller waited for the child"
        );

        let deadline = Instant::now() + Duration::from_secs(30);
        while proc_entry.try_exists().expect("stat the proc entry") {
            assert!(Instant::now() < deadline, "the child was never reaped");
            std::thread::sleep(Duration::from_millis(10));
        }
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
