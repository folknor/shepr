//! Local server rendezvous for the TUI and the SSH bridge: find the running
//! server, or start one and wait until it proves itself.
//!
//! The server is the `shepr-server` executable installed beside the running
//! `shepr`. A launch is:
//!
//! 1. Probe the server socket and ask a live listener for status. Gone permits
//!    launch, Starting and Stopping are waited out, Running carries the build
//!    identity, and Unresponsive is a failure. An inaccessible socket or a
//!    listener served by another user never permits a successor.
//! 2. Only for the build profile's own runtime address: take the launch lock
//!    in the runtime directory, so simultaneous first clients start one
//!    server, and probe again under it.
//! 3. Start `shepr-server --client-spawned` detached in its own session, its
//!    stderr going to a boot log in the runtime directory until its own
//!    logging is up, and hold it in a
//!    guard that kills its process group on every unsuccessful exit.
//! 4. Poll until the child answers a status request with this build's
//!    identity, noticing a child that dies on the way. Only then is the guard
//!    disarmed and the lock released.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use shepr_api::schema::SiblingServerJson;
use shepr_core::env::EnvVar;
use shepr_platform::SpawnedDaemon;
use shepr_platform::ipc::FlockLock;
use tracing::info;

use crate::daemon_exit::DaemonExit;
use crate::failure::RemoteFailureClass;
use crate::guidance;
use crate::invocation::{SERVER_BINARY_NAME, ServerInvocation, parse_server_version_line};
use crate::limits::{
    BOOT_LOG_MAX_BYTES, DAEMON_RESTART_INTERVAL, LAUNCH_LOCK_WAIT_GRACE,
    SIBLING_VERSION_OUTPUT_BYTES, SIBLING_VERSION_TIMEOUT, SOCKET_POLL_INTERVAL,
    STATUS_REQUEST_TIMEOUT,
};
use crate::status::{RuntimeStatus, ServerPresence};

pub use crate::limits::SERVER_READY_TIMEOUT;

/// The launch lock inside the runtime directory. It is never removed, so
/// every contender locks the same inode.
const LAUNCH_LOCK_FILE_NAME: &str = "launch.lock";

/// Where a launched server's stderr goes while it boots, inside the runtime
/// directory.
const BOOT_LOG_FILE_NAME: &str = "server-boot.log";

/// A local launch failure retains its cause and the full operator diagnostic.
#[derive(Debug)]
pub enum LaunchError {
    Unresponsive {
        message: String,
    },
    DifferentBuild {
        status: RuntimeStatus,
        message: String,
    },
    OverrideMissing {
        message: String,
    },
    TransitionTimeout {
        timeout: Duration,
        message: String,
    },
    DaemonFailed {
        class: DaemonExit,
        status: ExitStatus,
        message: String,
    },
    BootLogOverflow {
        message: String,
    },
    BootTimeout {
        timeout: Duration,
        occupant_only: bool,
        message: String,
    },
    SiblingBuildMismatch {
        status: RuntimeStatus,
        message: String,
    },
    Executable(io::Error),
    LaunchLock(io::Error),
    /// Deliberately one variant: it carries the `io::Error` of the version-line
    /// read and of the daemon spawn, and nothing branches on it beyond
    /// `kind()`, so splitting it would add variants no caller distinguishes.
    /// The failures a caller does tell apart, `Executable` and `LaunchLock`,
    /// are already separate variants.
    Io(io::Error),
}

impl LaunchError {
    pub fn kind(&self) -> io::ErrorKind {
        match self {
            Self::TransitionTimeout { .. } | Self::BootTimeout { .. } => io::ErrorKind::TimedOut,
            Self::OverrideMissing { .. } => io::ErrorKind::NotFound,
            Self::Executable(error) | Self::LaunchLock(error) | Self::Io(error) => error.kind(),
            _ => io::ErrorKind::Other,
        }
    }

    /// What this failure asks of the operator when it ends the SSH bridge on
    /// a remote host, which reports it to the client as this class.
    ///
    /// Every variant is classified here, with no wildcard arm, so a new one
    /// must be. A failure the operator text tells someone to fix on the host
    /// (an install, a server that will not answer, a refused configuration, a
    /// socket override naming no server) needs repair; a wait that ran out
    /// while a server was starting or another launcher held the lock is
    /// retried.
    pub fn remote_failure_class(&self) -> RemoteFailureClass {
        use RemoteFailureClass::{Repair, Retry};
        match self {
            Self::Unresponsive { .. }
            | Self::DifferentBuild { .. }
            | Self::OverrideMissing { .. }
            | Self::BootLogOverflow { .. }
            | Self::SiblingBuildMismatch { .. }
            | Self::Executable(_) => Repair,
            Self::TransitionTimeout { .. } | Self::BootTimeout { .. } => Retry,
            Self::DaemonFailed { class, .. } => match class {
                DaemonExit::ConfigRefused | DaemonExit::Failed => Repair,
                // `launch_with` waits out a daemon that gave way to another
                // server rather than failing on it; were one to fail the
                // launch, the occupant it met is what the next attempt finds.
                DaemonExit::Clean | DaemonExit::AlreadyRunning => Retry,
            },
            // A timeout, or a peer that went away mid-answer (a server dying
            // or restarting while the boot probe talks to it), is transient;
            // any other IO failure is the host's to fix.
            Self::LaunchLock(error) | Self::Io(error) => match error.kind() {
                io::ErrorKind::TimedOut
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::BrokenPipe => Retry,
                _ => Repair,
            },
        }
    }
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unresponsive { message }
            | Self::DifferentBuild { message, .. }
            | Self::OverrideMissing { message }
            | Self::TransitionTimeout { message, .. }
            | Self::DaemonFailed { message, .. }
            | Self::BootLogOverflow { message }
            | Self::BootTimeout { message, .. }
            | Self::SiblingBuildMismatch { message, .. } => f.write_str(message),
            Self::Executable(error) | Self::LaunchLock(error) | Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for LaunchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Executable(error) | Self::LaunchLock(error) | Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for LaunchError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

// IO-only presentation boundaries retain the OS kind and full diagnostic;
// launch policy must inspect LaunchError before converting it.
impl From<LaunchError> for io::Error {
    fn from(error: LaunchError) -> Self {
        io::Error::new(error.kind(), error.to_string())
    }
}

/// A direct client checks the build before attaching. An SSH bridge accepts a
/// running server of another build and answers the client with that build's
/// preamble itself, so the client still reports a typed mismatch.
#[derive(Clone, Copy)]
pub enum BuildCheck {
    BeforeAttach,
    AtClientHandshake,
}

/// Ensures a server is listening, with the caller's build-check policy, and
/// returns the status of the server it accepted: the one probed, or the one
/// it launched and verified.
///
/// A server this call starts is verified to be this build before it returns,
/// whatever the policy: the policy governs only a server that was already
/// running.
pub fn ensure_running(
    paths: &shepr_paths::AppPaths,
    timeout: Duration,
    build_check: BuildCheck,
) -> Result<RuntimeStatus, LaunchError> {
    match probe_server(paths)? {
        Probed::Running(status) => {
            info!("server already running");
            return accept_running(paths, status, build_check);
        }
        Probed::Unresponsive => return Err(unresponsive_error(paths)),
        Probed::Starting | Probed::Stopping if !paths.server_address().is_runtime_address() => {
            return wait_for_overridden_server(paths, timeout, build_check);
        }
        Probed::NoServer | Probed::Starting | Probed::Stopping => {}
    }
    require_own_runtime_address(paths)?;
    let server = server_executable().map_err(LaunchError::Executable)?;

    let _lock = acquire_launch_lock(paths, timeout.saturating_add(LAUNCH_LOCK_WAIT_GRACE))
        .map_err(LaunchError::LaunchLock)?;
    // One budget covers every socket transition while this client owns the
    // launch lock; a server that repeatedly starts and releases cannot reset it.
    // clock-io-ok: the launch budget measures real elapsed waiting on the socket
    let transition_deadline = Instant::now() + timeout;
    // A client that held the lock before us may have finished its launch.
    let mut probed = probe_server(paths)?;
    loop {
        match probed {
            Probed::Running(status) => {
                info!("server started by another client");
                return accept_running(paths, status, build_check);
            }
            Probed::Unresponsive => return Err(unresponsive_error(paths)),
            Probed::NoServer => break,
            Probed::Starting | Probed::Stopping => {
                info!("the server socket is in transition; waiting for it to settle");
                // clock-io-ok: the launch budget measures real elapsed waiting
                if transition_deadline
                    .saturating_duration_since(Instant::now())
                    .is_zero()
                {
                    return Err(server_transition_timeout(paths, timeout));
                }
                probed =
                    wait_for_server_socket_to_settle_until(paths, transition_deadline, timeout)?;
            }
        }
    }

    info!(server = %server.display(), "no server running, starting the server daemon");
    let status = launch_daemon(paths, &server, timeout)?;
    accept_running(paths, status, build_check)
}

/// What is running at the local server address, without ever starting a server:
/// the status of a stable server that answers, or `None` when no restartable
/// server is present or the server is starting or stopping. The pre-TUI
/// restart offer reads a different-build server through this. A live
/// listener that does not answer, or a socket that cannot be judged, is an
/// error, as it is for a launch.
pub fn running_server_status(paths: &shepr_paths::AppPaths) -> io::Result<Option<RuntimeStatus>> {
    match probe_server(paths)? {
        Probed::Running(status) => Ok(Some(status)),
        // There is no stable server status to offer; the launch that follows
        // resolves the transition under the profile lock.
        Probed::NoServer | Probed::Starting | Probed::Stopping => Ok(None),
        Probed::Unresponsive => Err(io::Error::other(unresponsive_error(paths).to_string())),
    }
}

// ---------------------------------------------------------------------------
// Probing
// ---------------------------------------------------------------------------

/// What a probe of the local server found.
#[derive(Debug)]
enum Probed {
    /// The socket has no live listener.
    NoServer,
    /// The socket answers that the server is still restoring.
    Starting,
    /// A server listens and answered a status request.
    Running(RuntimeStatus),
    /// Something listens but gave no status answer within the deadline.
    Unresponsive,
    /// The server is stopping and no longer accepts TUI connections.
    Stopping,
}

fn probe_server(paths: &shepr_paths::AppPaths) -> io::Result<Probed> {
    probe_server_at(paths.server_address().socket())
}

/// Probes the server socket, and follows a live one with a bounded status
/// request rather than trusting that a connect succeeded. An unreachable
/// socket (permission, a non-socket in the way, a symlink loop) and a listener
/// served by another user are errors: neither proves absence, so neither may
/// lead to a second server. The status request itself checks who serves the
/// socket before writing to it.
fn probe_server_at(socket: &Path) -> io::Result<Probed> {
    Ok(
        match crate::status::read_server_presence_at(socket, STATUS_REQUEST_TIMEOUT)? {
            ServerPresence::Gone => Probed::NoServer,
            ServerPresence::Starting(_) => Probed::Starting,
            ServerPresence::Running(status) => Probed::Running(status),
            ServerPresence::Stopping(_) => Probed::Stopping,
            ServerPresence::Unresponsive => Probed::Unresponsive,
        },
    )
}

/// Waits through a server transition until the server socket disappears or
/// a different stable probe result appears. The launcher holds its profile
/// lock while waiting, so another shepr client cannot start a competing
/// successor in this interval.
fn wait_for_server_socket_to_settle_until(
    paths: &shepr_paths::AppPaths,
    deadline: Instant,
    timeout: Duration,
) -> Result<Probed, LaunchError> {
    // clock-io-ok: bounds a wait on another process's real socket.
    loop {
        match probe_server(paths)? {
            stable @ (Probed::NoServer | Probed::Running(_) | Probed::Unresponsive) => {
                return Ok(stable);
            }
            Probed::Starting | Probed::Stopping => {}
        }
        // clock-io-ok: the same real-socket wait.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(server_transition_timeout(paths, timeout));
        }
        std::thread::sleep(SOCKET_POLL_INTERVAL.min(remaining));
    }
}

fn server_transition_timeout(paths: &shepr_paths::AppPaths, timeout: Duration) -> LaunchError {
    LaunchError::TransitionTimeout {
        timeout,
        message: format!(
            "the shepr server at {} did not finish starting or release its socket within {}ms",
            paths.server_address().socket().display(),
            timeout.as_millis()
        ),
    }
}

/// An override names an existing server and can never launch one. If that
/// server is transitioning, wait for it to become attachable or disappear
/// before reporting the stable result.
fn wait_for_overridden_server(
    paths: &shepr_paths::AppPaths,
    timeout: Duration,
    build_check: BuildCheck,
) -> Result<RuntimeStatus, LaunchError> {
    // clock-io-ok: the launch budget measures real elapsed waiting on the socket
    let deadline = Instant::now() + timeout;
    let mut settled = wait_for_server_socket_to_settle_until(paths, deadline, timeout)?;
    loop {
        match settled {
            Probed::Running(status) => return accept_running(paths, status, build_check),
            Probed::NoServer => return Err(no_server_at_override(paths)),
            Probed::Unresponsive => return Err(unresponsive_error(paths)),
            Probed::Starting | Probed::Stopping => {
                // The shared wait returns only settled states; should that
                // change, wait again within the original deadline.
                settled = wait_for_server_socket_to_settle_until(paths, deadline, timeout)?;
            }
        }
    }
}

fn unresponsive_error(paths: &shepr_paths::AppPaths) -> LaunchError {
    LaunchError::Unresponsive {
        message: format!(
            "a shepr server is listening at {}, but it is not answering status requests, so its build cannot be confirmed and no second server is started.\n\n{}\nIf that fails, stop the server process manually.",
            paths.server_address().socket().display(),
            build_mismatch_guidance(paths)
        ),
    }
}

/// Applies the caller's policy to a running server's build.
fn accept_running(
    paths: &shepr_paths::AppPaths,
    status: RuntimeStatus,
    build_check: BuildCheck,
) -> Result<RuntimeStatus, LaunchError> {
    if status.build_id.is_this_build() {
        return Ok(status);
    }
    match build_check {
        BuildCheck::BeforeAttach => Err(running_build_mismatch(paths, &status)),
        BuildCheck::AtClientHandshake => Ok(status),
    }
}

fn running_build_mismatch(paths: &shepr_paths::AppPaths, status: &RuntimeStatus) -> LaunchError {
    let summary = if paths.server_address().is_runtime_address() {
        "the running shepr server is a different build; restart it before attaching."
    } else {
        "the running shepr server is a different build, and this client cannot start a replacement at the selected socket override."
    };
    LaunchError::DifferentBuild {
        status: status.clone(),
        message: format!(
            "{summary}\n\nserver: v{} build {}\nclient: v{} build {}\n\n{}",
            status.version,
            status.build_id,
            shepr_protocol::build_version(),
            shepr_protocol::BUILD_ID,
            build_mismatch_guidance(paths)
        ),
    }
}

fn build_mismatch_guidance(paths: &shepr_paths::AppPaths) -> String {
    guidance::build_mismatch_guidance(paths.server_address())
}

/// A client starts a server only for its profile's own runtime address. A
/// socket override names a server that is already running (a pane's own, or a
/// test's); a server started for it would only meet the data directory lease
/// the profile's real server holds.
fn require_own_runtime_address(paths: &shepr_paths::AppPaths) -> Result<(), LaunchError> {
    let address = paths.server_address();
    if address.is_runtime_address() {
        return Ok(());
    }
    Err(no_server_at_override(paths))
}

fn no_server_at_override(paths: &shepr_paths::AppPaths) -> LaunchError {
    let address = paths.server_address();
    let selected_by = EnvVar::SheprSocketPath;
    LaunchError::OverrideMissing {
        message: format!(
            "no shepr server is running at {}, which {selected_by} selects. A client starts a server only for its own runtime address ({}); a socket override names a server that is already running.",
            address.socket().display(),
            paths.runtime_dir().display()
        ),
    }
}

// ---------------------------------------------------------------------------
// The server executable
// ---------------------------------------------------------------------------

/// The `shepr-server` installed beside the running client.
///
/// The client is found through `launch_executable`, which follows an
/// executable an install replaced on disk, and only its file name is swapped:
/// no `PATH` lookup and no environment override chooses the server. A missing
/// or non-executable sibling is an install error naming the path.
pub fn server_executable() -> io::Result<PathBuf> {
    let client = shepr_platform::launch_executable().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to determine the shepr executable path: {error}"),
        )
    })?;
    sibling_server_executable(&client)
}

fn sibling_server_executable(client: &Path) -> io::Result<PathBuf> {
    let server = client.with_file_name(SERVER_BINARY_NAME);
    let install_hint = format!(
        "shepr starts its server from the same directory as itself; install shepr and {SERVER_BINARY_NAME} together (`brokkr install`)"
    );
    match std::fs::metadata(&server) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a regular file; {install_hint}", server.display()),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{SERVER_BINARY_NAME} was not found at {}; {install_hint}",
                    server.display()
                ),
            ));
        }
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("cannot inspect {}: {error}", server.display()),
            ));
        }
    }
    if !shepr_platform::has_execute_access(&server) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not executable; {install_hint}", server.display()),
        ));
    }
    Ok(server)
}

/// The identity of the `shepr-server` installed beside the running client, for
/// `status client`: its resolved path and the build its `--version` reports, or
/// why neither could be had. It never fails, since a broken installation is what
/// the report exists to show; a remote client's discovery reads it to check
/// that the installed pair is one build.
pub fn sibling_server_status() -> SiblingServerJson {
    let client = match shepr_platform::launch_executable() {
        Ok(client) => client,
        Err(error) => {
            return SiblingServerJson {
                binary: None,
                identity: Err(format!(
                    "failed to determine the shepr executable path: {error}"
                )),
            };
        }
    };
    let binary = client.with_file_name(SERVER_BINARY_NAME);
    let identity = sibling_server_executable(&client)
        .and_then(|server| read_server_version_line(&server, SIBLING_VERSION_TIMEOUT))
        .and_then(|line| {
            parse_server_version_line(&line).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unrecognized --version output {:?}", line.trim()),
                )
            })
        });
    match identity {
        Ok((version, build_id)) => SiblingServerJson {
            binary: Some(binary.display().to_string()),
            identity: Ok(shepr_protocol::BuildVersion { version, build_id }),
        },
        Err(error) => SiblingServerJson {
            binary: Some(binary.display().to_string()),
            identity: Err(error.to_string()),
        },
    }
}

/// Runs `server --version` under a deadline and returns its first output line.
/// A child that outlives the deadline is killed and reaped.
fn read_server_version_line(server: &Path, timeout: Duration) -> io::Result<String> {
    use std::io::Read as _;

    let mut command = shepr_platform::child_command(server, Path::new("/"));
    command
        .args(ServerInvocation::Version.args())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to run {} --version: {error}", server.display()),
        )
    })?;
    let deadline = real_now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                drop(child.kill());
                drop(child.wait());
                return Err(error);
            }
        }
        if real_now() >= deadline {
            drop(child.kill());
            drop(child.wait());
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{} --version did not finish within {}s",
                    server.display(),
                    timeout.as_secs()
                ),
            ));
        }
        std::thread::sleep(SOCKET_POLL_INTERVAL);
    };
    if !status.success() {
        return Err(io::Error::other(format!(
            "{} --version failed ({status})",
            server.display()
        )));
    }
    let mut output = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(SIBLING_VERSION_OUTPUT_BYTES)
            .read_to_end(&mut output)?;
    }
    let text = String::from_utf8_lossy(&output);
    Ok(text.lines().next().unwrap_or_default().to_owned())
}

// ---------------------------------------------------------------------------
// The launch lock
// ---------------------------------------------------------------------------

/// Takes the launch lock of the profile's runtime directory, polling without
/// blocking for at most `wait`.
///
/// The lock is keyed to the profile, not to a socket path: socket overrides
/// move only the socket, so every server of one profile competes for the same
/// data directory lease anyway.
fn acquire_launch_lock(paths: &shepr_paths::AppPaths, wait: Duration) -> io::Result<FlockLock> {
    shepr_platform::create_private_directory_all(paths.runtime_dir()).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot create the runtime directory {}: {error}",
                paths.runtime_dir().display()
            ),
        )
    })?;
    acquire_launch_lock_with(
        &paths.runtime_dir().join(LAUNCH_LOCK_FILE_NAME),
        wait,
        &mut real_now,
        &mut std::thread::sleep,
    )
}

fn acquire_launch_lock_with(
    lock_path: &Path,
    wait: Duration,
    now: &mut impl FnMut() -> Instant,
    sleep: &mut impl FnMut(Duration),
) -> io::Result<FlockLock> {
    let deadline = now() + wait;
    loop {
        match shepr_platform::ipc::acquire_flock_lock(
            lock_path,
            shepr_platform::ipc::LockWait::FailIfHeld,
        ) {
            Ok(lock) => return Ok(lock),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "another shepr is still starting the server: it has held {} for {}s",
                            lock_path.display(),
                            wait.as_secs()
                        ),
                    ));
                }
                sleep(SOCKET_POLL_INTERVAL);
            }
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "cannot take the launch lock {}: {error}",
                        lock_path.display()
                    ),
                ));
            }
        }
    }
}

/// The production clock of the launch loops.
fn real_now() -> Instant {
    // clock-io-ok: the production adapter measures actual launch wait time.
    Instant::now()
}

// ---------------------------------------------------------------------------
// Starting the daemon
// ---------------------------------------------------------------------------

/// The files a launch names in its failure messages.
struct LaunchFiles<'a> {
    /// The `shepr-server` being started.
    server: &'a Path,
    /// Where its stderr goes while it boots.
    boot_log: &'a Path,
    /// Where it logs once tracing is up.
    server_log: &'a Path,
}

fn launch_daemon(
    paths: &shepr_paths::AppPaths,
    server: &Path,
    timeout: Duration,
) -> Result<RuntimeStatus, LaunchError> {
    let boot_log = paths.runtime_dir().join(BOOT_LOG_FILE_NAME);
    let server_log = paths.server_log();
    let working_dir = server_daemon_working_dir(paths);
    launch_with(
        &LaunchFiles {
            server,
            boot_log: &boot_log,
            server_log: &server_log,
        },
        timeout,
        |stderr| {
            // Keep startup attached to the child guard below. A transient
            // `systemd-run --user --scope` needs an active user manager and
            // waits synchronously for its command to exit, which would block
            // readiness checks; an asynchronous service launch would need a
            // different owner for startup failure cleanup. Without lingering,
            // a user scope also ends with the user manager after the last login.
            // `setsid` separates the terminal session, but logind's cgroup
            // policy still applies.
            let mut command =
                build_server_daemon_command(server, &working_dir, paths.current_dir(), paths);
            command.stderr(stderr);
            command.spawn()
        },
        || probe_server(paths),
        &mut real_now,
        &mut std::thread::sleep,
    )
}

/// Starts the daemon through `spawn` (handed the boot log as its stderr) and
/// polls `probe` until the daemon answers with this build's identity.
///
/// Every way out but that answer kills and reaps the daemon's process group:
/// a probe failure, the timeout, a build mismatch and a child that died. The
/// daemon exiting because another server already holds the runtime is not a
/// failure yet: the occupant is what the client will attach to, so polling
/// goes on for it until the deadline. An occupant of another build that
/// answers is returned for the caller's build-check policy; one that answers
/// that it is stopping is never returned, only polled past until its socket
/// go. While nothing listens, such a daemon is started again every
/// [`DAEMON_RESTART_INTERVAL`]: the holder may be a server that is still
/// starting before its socket bind, or one that has released its socket and
/// still holds its lease. The launch is owed a daemon of its own once the
/// lease is free.
fn launch_with(
    files: &LaunchFiles<'_>,
    timeout: Duration,
    mut spawn: impl FnMut(Stdio) -> io::Result<Child>,
    mut probe: impl FnMut() -> io::Result<Probed>,
    now: &mut impl FnMut() -> Instant,
    sleep: &mut impl FnMut(Duration),
) -> Result<RuntimeStatus, LaunchError> {
    let boot_log = shepr_platform::open_boot_log(files.boot_log).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot open the server boot log {}: {error}",
                files.boot_log.display()
            ),
        )
    })?;
    // The launcher's own handle on the log, to bound its size while the daemon
    // boots and to empty it once the daemon is up.
    let boot_log_handle = boot_log.try_clone().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot open the server boot log {}: {error}",
                files.boot_log.display()
            ),
        )
    })?;
    let spawn_daemon = |spawn: &mut dyn FnMut(Stdio) -> io::Result<Child>| {
        let stderr = boot_log.try_clone().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot open the server boot log {}: {error}",
                    files.boot_log.display()
                ),
            )
        })?;
        let child = spawn(Stdio::from(stderr)).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to start {}: {error}", files.server.display()),
            )
        })?;
        info!(pid = child.id(), "server daemon spawned");
        Ok::<_, io::Error>(SpawnedDaemon::new(child))
    };
    let mut daemon = spawn_daemon(&mut spawn)?;
    let mut last_spawn = now();

    let deadline = now() + timeout;
    let mut exited: Option<ExitStatus> = None;
    loop {
        if exited.is_none() {
            exited = daemon.try_wait().map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot check the server daemon: {error}"),
                )
            })?;
            if let Some(status) = exited {
                info!(%status, "server daemon exited during boot");
            }
        }

        // A daemon that failed outright takes precedence over what the probe
        // says, unless a server is up: the failure may be a race that some
        // other server won.
        let failed = exited
            .filter(|status| DaemonExit::from_code(status.code()) != DaemonExit::AlreadyRunning);
        let probed = match probe() {
            Ok(probed) => probed,
            Err(error) => {
                return Err(failed.map_or_else(
                    || LaunchError::Io(error),
                    |status| boot_failure(files, status),
                ));
            }
        };
        let nothing_listens = matches!(probed, Probed::NoServer);
        if let Probed::Running(status) = probed {
            if status.build_id.is_this_build() {
                // Nothing in the boot log matters once the daemon is up, and
                // the daemon keeps the descriptor for its whole life.
                if let Err(error) = boot_log_handle.set_len(0) {
                    tracing::debug!(%error, "could not empty the server boot log");
                }
                daemon.disarm();
                return Ok(status);
            }
            if exited.is_none() {
                if daemon
                    .process_id()
                    .is_some_and(|pid| boot_id_process_id(&status.boot_id) == Some(pid))
                {
                    return Err(sibling_build_mismatch(files, &status));
                }
                // A directly launched server can bind after our second probe
                // and before this daemon reaches its own bind. Its answer is
                // not proof that the daemon we started has the wrong build.
                // Keep polling until that daemon reports AlreadyRunning or
                // otherwise exits; then this external occupant can be handed
                // back without the launch guard killing a healthy process.
            } else {
                // The daemon gave way to a different build, so this is the
                // external occupant the caller's build-check policy handles.
                return Ok(status);
            }
        }
        if let Some(status) = failed {
            return Err(boot_failure(files, status));
        }
        if boot_log_handle
            .metadata()
            .is_ok_and(|metadata| metadata.len() > BOOT_LOG_MAX_BYTES)
        {
            return Err(boot_log_overflow(files));
        }
        let current = now();
        if current >= deadline {
            return Err(boot_timeout(files, timeout, exited.is_some()));
        }
        // The daemon gave way and nothing listens, so what it met was not a
        // server that will answer: a holder still booting before its socket
        // bind, or an older server that released its socket before its lease.
        // A server of this build retires its lease first. Retry; the
        // lease keeps from ever sharing the directory with the holder. A
        // holder that is a live, healthy server listens, which the probe
        // above turns into an answer instead of reaching here.
        if exited.is_some()
            && nothing_listens
            && current.saturating_duration_since(last_spawn) >= DAEMON_RESTART_INTERVAL
        {
            info!(
                "the server daemon found the data directory held while nothing listens, starting it again"
            );
            daemon = spawn_daemon(&mut spawn)?;
            last_spawn = current;
            exited = None;
            continue;
        }
        sleep(SOCKET_POLL_INTERVAL);
    }
}

/// The Linux process id in a canonical boot identity; `None` when its number
/// is outside the valid Linux pid range.
fn boot_id_process_id(boot_id: &shepr_protocol::BootId) -> Option<shepr_platform::Pid> {
    shepr_platform::Pid::new(boot_id.process_id()?)
}

/// The daemon exited during boot: how, and what it printed.
fn boot_failure(files: &LaunchFiles<'_>, status: ExitStatus) -> LaunchError {
    let class = DaemonExit::from_code(status.code());
    let mut message = format!(
        "{SERVER_BINARY_NAME} {} ({status})",
        class.describe_boot_end()
    );
    append_boot_log(&mut message, files);
    LaunchError::DaemonFailed {
        class,
        status,
        message,
    }
}

/// The daemon printed more than [`BOOT_LOG_MAX_BYTES`] while booting; the
/// caller's guard stops it.
fn boot_log_overflow(files: &LaunchFiles<'_>) -> LaunchError {
    let mut message = format!(
        "{SERVER_BINARY_NAME} wrote more than {BOOT_LOG_MAX_BYTES} bytes to its boot log while starting and was stopped"
    );
    append_boot_log(&mut message, files);
    LaunchError::BootLogOverflow { message }
}

/// The daemon did not answer with this build's identity in time.
fn boot_timeout(files: &LaunchFiles<'_>, timeout: Duration, occupant_only: bool) -> LaunchError {
    let mut message = if occupant_only {
        format!(
            "{SERVER_BINARY_NAME} found another server already running, but that server did not answer a status request within {}s",
            timeout.as_secs()
        )
    } else {
        format!(
            "{SERVER_BINARY_NAME} did not become ready within {}s and was stopped",
            timeout.as_secs()
        )
    };
    append_boot_log(&mut message, files);
    LaunchError::BootTimeout {
        timeout,
        occupant_only,
        message,
    }
}

fn append_boot_log(message: &mut String, files: &LaunchFiles<'_>) {
    match shepr_platform::read_boot_log_tail(files.boot_log) {
        Ok(tail) if !tail.is_empty() => message.push_str(&format!(
            "\nserver output ({}):\n{tail}",
            files.boot_log.display()
        )),
        Ok(_) => message.push_str(&format!(
            "\nthe server printed nothing during boot ({})",
            files.boot_log.display()
        )),
        Err(error) => message.push_str(&format!(
            "\ncould not read the server boot log {}: {error}",
            files.boot_log.display()
        )),
    }
    message.push_str(&format!(
        "\nonce it is running, the server logs to {}",
        files.server_log.display()
    ));
}

/// The daemon this client just started answered as another build, so the
/// installed pair is inconsistent.
fn sibling_build_mismatch(files: &LaunchFiles<'_>, status: &RuntimeStatus) -> LaunchError {
    LaunchError::SiblingBuildMismatch {
        status: status.clone(),
        message: format!(
            "{} is a different build than this shepr and was stopped; install shepr and {SERVER_BINARY_NAME} together (`brokkr install`).\n\nserver: v{} build {}\nclient: v{} build {}",
            files.server.display(),
            status.version,
            status.build_id,
            shepr_protocol::build_version(),
            shepr_protocol::BUILD_ID
        ),
    }
}

/// The working directory the server daemon runs in: the user's home
/// directory, or `/` when there is none. Not the launching shell's directory:
/// the daemon outlives that shell, and inheriting it would pin the directory
/// for the server's whole life (an unmount fails with EBUSY, a deleted
/// directory stays referenced). The directory the user launched from still
/// reaches the server, as `SHEPR_STARTUP_CWD`, and is the server's resolved
/// current directory (`AppPaths::resolve_for_server`): new terminals,
/// `new_terminal_cwd = "current"` and a relative `new_terminal_cwd` resolve
/// against it, not against this working directory. Home rather than `/` in
/// case a launch hands over no directory, since the server then falls back
/// to its own.
fn server_daemon_working_dir(paths: &shepr_paths::AppPaths) -> PathBuf {
    paths
        .home_dir()
        .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
}

/// The command that starts the server daemon, fully detached:
/// - runs in its own session (setsid), so it survives the client exiting and
///   leads a process group the launch guard can kill as a whole;
/// - stdin and stdout are `/dev/null`; stderr is `/dev/null` here and the
///   launch replaces it with the boot log;
/// - inherits the surrounding environment and gets the already-resolved socket
///   target, including removals for inherited overrides that were superseded.
fn build_server_daemon_command(
    exe: &Path,
    working_dir: &Path,
    startup_cwd: Option<&Path>,
    paths: &shepr_paths::AppPaths,
) -> Command {
    let mut command = shepr_platform::child_command(exe, working_dir);
    command
        .args(
            ServerInvocation::Serve {
                client_spawned: true,
            }
            .args(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    shepr_platform::detach_server_daemon_command(&mut command);

    // A private daemon-start hint that seeds a fresh headless server from the
    // directory where the user ran `shepr`.
    if let Some(startup_cwd) = startup_cwd {
        command.env(EnvVar::SheprStartupCwd, startup_cwd);
    } else {
        command.env_remove(EnvVar::SheprStartupCwd);
    }

    paths.server_address().apply_to_child_command(&mut command);

    command
}

#[cfg(test)]
#[path = "local_server_tests.rs"]
mod local_server_tests;
