use super::*;

use super::process::{
    PIPE_DRAIN_GRACE, PipeCapture, PipeEcho, SSH_STDERR_CAPTURE_LIMIT, SSH_STDOUT_CAPTURE_LIMIT,
    wait_with_output_timeout,
};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};

pub(super) const NONINTERACTIVE_SSH_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
pub(super) const NONINTERACTIVE_SSH_STDERR_LIMIT: usize = SSH_STDERR_CAPTURE_LIMIT;

#[derive(Clone)]
pub(crate) struct ManagedSshOptions {
    pub(super) config_path: PathBuf,
    pub(super) control_path: Option<PathBuf>,
    // Bridge workers may launch SSH after the helper that created this config
    // has gone away. The last options owner removes only the temporary config.
    pub(super) _directory: Arc<ManagedSshConfigDirectory>,
}

pub(super) struct ManagedSshConfig {
    pub(super) options: ManagedSshOptions,
}

pub(super) struct ManagedSshConfigDirectory {
    path: PathBuf,
    // Declared after `path` and dropped after `Drop::drop` has removed the directory,
    // so the exit sweep only ever sees directories that still exist.
    _teardown: TeardownRegistration,
}

impl ManagedSshConfigDirectory {
    pub(super) fn new(path: PathBuf) -> Self {
        let teardown = SSH_TEARDOWN.register(TeardownResource::Directory(path.clone()));
        Self {
            path,
            _teardown: teardown,
        }
    }
}

impl Drop for ManagedSshConfigDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Files the SSH machinery leaves in the temp directory while it runs: bridge
/// sockets and temporary ssh config directories.
pub(super) enum TeardownResource {
    Socket {
        path: PathBuf,
        identity: shepr_platform::ipc::SocketFileIdentity,
    },
    Directory(PathBuf),
}

impl TeardownResource {
    pub(super) fn remove(&self) {
        match self {
            Self::Socket { path, identity } => {
                let _ = shepr_platform::ipc::remove_socket_file_if_owned(path, identity);
            }
            Self::Directory(path) => {
                let _ = fs::remove_dir_all(path);
            }
        }
    }
}

/// Tracks every live temp-directory resource so the process can remove them before
/// it exits.
///
/// Their owners normally remove them on drop, but in a client those owners live on
/// endpoint writer threads and in connection attempts on blocking tasks. Both are
/// still unwinding when the main thread returns from the client loop, and process
/// exit does not wait for them, which leaked sockets and config directories.
pub(super) struct TeardownRegistry {
    pending: std::sync::Mutex<Vec<(u64, TeardownResource)>>,
    changed: std::sync::Condvar,
    next_id: std::sync::atomic::AtomicU64,
}

pub(super) static SSH_TEARDOWN: TeardownRegistry = TeardownRegistry::new();

impl TeardownRegistry {
    pub(super) const fn new() -> Self {
        Self {
            pending: std::sync::Mutex::new(Vec::new()),
            changed: std::sync::Condvar::new(),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(u64, TeardownResource)>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn register(&'static self, resource: TeardownResource) -> TeardownRegistration {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock().push((id, resource));
        TeardownRegistration { registry: self, id }
    }

    /// Gives owners that are already dropping up to `grace` to finish, then
    /// removes whatever is still registered.
    pub(super) fn release_all(&self, grace: Duration) {
        let deadline = Instant::now() + grace;
        let mut pending = self.lock();
        while !pending.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            pending = match self.changed.wait_timeout(pending, deadline - now) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        for (_, resource) in pending.drain(..) {
            resource.remove();
        }
    }
}

pub(super) struct TeardownRegistration {
    registry: &'static TeardownRegistry,
    id: u64,
}

impl Drop for TeardownRegistration {
    fn drop(&mut self) {
        let mut pending = self.registry.lock();
        pending.retain(|(id, _)| *id != self.id);
        drop(pending);
        self.registry.changed.notify_all();
    }
}

/// Removes the SSH bridge sockets and temporary ssh config directories this process
/// still owns. Call it once, after the client loop has returned and immediately
/// before the process exits (including through `std::process::exit`).
///
/// Owners that are mid-teardown get up to `grace` to finish cleanly; anything left
/// after that is removed directly. SSH children are not waited for: a bridge's ssh
/// sees its stdin close when this process exits and ends on its own.
pub fn release_ssh_resources_before_exit(grace: Duration) {
    SSH_TEARDOWN.release_all(grace);
}

/// Keep this owner alive until the child has exited: OpenSSH reads its temporary
/// config after spawn. Dropping it never stops the shared authenticated master.
pub struct SshAuthenticationCommand {
    pub command: Command,
    _config: ManagedSshConfig,
}

pub fn ssh_authentication_command(
    paths: &shepr_config::AppPaths,
    target: &SshTarget,
    settings: super::SavedSshSettings,
) -> io::Result<SshAuthenticationCommand> {
    if !settings.manage_ssh_config {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interactive SSH recovery requires remote.manage_ssh_config=true",
        ));
    }
    let config = write_managed_ssh_config(target.as_str(), paths, SshControlDir::runtime(paths)?)?;
    Ok(authentication_command_with_config(target, config))
}

pub(super) fn authentication_command_with_config(
    target: &SshTarget,
    config: ManagedSshConfig,
) -> SshAuthenticationCommand {
    let mut command = Command::new("ssh");
    apply_managed_ssh_options(&mut command, Some(&config.options));
    command
        .env(shepr_core::env::ChildEnv::SshAskpassRequire, "never")
        .env_remove(shepr_core::env::ChildEnv::SshAskpass)
        .arg("-o")
        .arg("BatchMode=no")
        .arg("-o")
        .arg("StrictHostKeyChecking=yes")
        .arg("-o")
        .arg("NumberOfPasswordPrompts=3")
        .arg("-T")
        .arg(target.as_str())
        .arg("exit");
    SshAuthenticationCommand {
        command,
        _config: config,
    }
}

pub(crate) struct RemoteSsh {
    target: SshTarget,
    session_name: String,
    managed_config: Option<ManagedSshConfig>,
    noninteractive: bool,
    /// When set, no noninteractive command runs past it: each one gets the shorter of its
    /// own timeout and the time left, and none starts once it has passed. A saved-machine
    /// connection attempt sets it so discovery cannot outlast the attempt's budget.
    attempt_deadline: Option<Instant>,
}

impl RemoteSsh {
    pub(super) fn new(
        target: SshTarget,
        manage_ssh_config: bool,
        session_name: String,
        paths: &shepr_config::AppPaths,
    ) -> io::Result<Self> {
        let control_dir = if manage_ssh_config {
            Some(SshControlDir::runtime(paths)?)
        } else {
            None
        };
        Self::with_control_dir(target, control_dir, session_name, paths)
    }

    /// As [`RemoteSsh::new`], with the managed config's control directory
    /// given: `None` leaves the user's SSH config unmanaged.
    pub(super) fn with_control_dir(
        target: SshTarget,
        control_dir: Option<SshControlDir<'_>>,
        session_name: String,
        paths: &shepr_config::AppPaths,
    ) -> io::Result<Self> {
        let managed_config = match control_dir {
            Some(control_dir) => Some(write_managed_ssh_config(
                target.as_str(),
                paths,
                control_dir,
            )?),
            None => None,
        };

        Ok(Self {
            target,
            session_name,
            managed_config,
            noninteractive: false,
            attempt_deadline: None,
        })
    }

    /// For long-lived callers that already hold the launch-time config.
    pub(crate) fn new_noninteractive_with(
        target: SshTarget,
        manage_ssh_config: bool,
        paths: &shepr_config::AppPaths,
    ) -> io::Result<Self> {
        let mut ssh = Self::new(
            target,
            manage_ssh_config,
            shepr_config::DEFAULT_SESSION_NAME.into(),
            paths,
        )?;
        ssh.noninteractive = true;
        Ok(ssh)
    }

    pub(super) fn set_session_name(&mut self, session_name: String) {
        self.session_name = session_name;
    }

    pub(super) fn session_name(&self) -> &str {
        &self.session_name
    }

    #[cfg(test)]
    pub(super) fn test_with_state(
        target: SshTarget,
        session_name: String,
        managed_config: Option<ManagedSshConfig>,
        noninteractive: bool,
    ) -> Self {
        Self {
            target,
            session_name,
            managed_config,
            noninteractive,
            attempt_deadline: None,
        }
    }

    pub(crate) fn set_attempt_deadline(&mut self, deadline: Option<Instant>) {
        self.attempt_deadline = deadline;
    }

    /// The timeout for the next noninteractive command, or `TimedOut` when the attempt
    /// deadline has already passed and no further command may start.
    pub(super) fn noninteractive_timeout(&self) -> io::Result<Duration> {
        let Some(deadline) = self.attempt_deadline else {
            return Ok(NONINTERACTIVE_SSH_COMMAND_TIMEOUT);
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(attempt_deadline_passed());
        }
        Ok(remaining.min(NONINTERACTIVE_SSH_COMMAND_TIMEOUT))
    }

    pub(super) fn target(&self) -> &str {
        self.target.as_str()
    }

    pub(super) fn destination(&self) -> String {
        format!("{} (session {})", self.target, self.session_name)
    }

    pub(crate) fn options(&self) -> Option<&ManagedSshOptions> {
        self.managed_config.as_ref().map(|config| &config.options)
    }

    pub(super) fn command(&self) -> Command {
        let mut command = self.base_command();
        if self.noninteractive {
            apply_noninteractive_ssh_options(&mut command);
        }
        command.arg("-T").arg(self.target.as_str());
        command
    }

    pub(super) fn base_command(&self) -> Command {
        let mut command = Command::new("ssh");
        apply_managed_ssh_options(&mut command, self.options());
        command
    }

    pub(super) fn sh_output(&self, script: &str) -> io::Result<Output> {
        let script = posix_remote_output_command(script);
        let timeout = self.noninteractive_timeout()?;
        let mut child = self
            .command()
            .arg("/bin/sh -s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        if !self.noninteractive {
            return normalize_remote_output(output_with_forwarded_stderr(
                child,
                Some(script.as_bytes()),
            )?);
        }

        let write_result = if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(script.as_bytes())
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ssh bootstrap stdin missing",
            ))
        };
        let output = wait_with_output_timeout(child, timeout)?;
        write_result?;
        normalize_remote_output(output)
    }

    pub(super) fn framed_user_shell_output(&self, remote_command: &str) -> io::Result<Output> {
        let mut command = self.command();
        command
            .arg(remote_command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = if self.noninteractive {
            let timeout = self.noninteractive_timeout()?;
            wait_with_output_timeout(command.spawn()?, timeout)
        } else {
            output_with_forwarded_stderr(command.spawn()?, None)
        }?;
        normalize_remote_output(output)
    }

    pub(super) fn posix_user_shell_output(&self, remote_command: &str) -> io::Result<Output> {
        self.framed_user_shell_output(&posix_remote_output_command(remote_command))
    }
}

// Only interactive setup uses this relay. Background probes retain their
// capture-only timeout path so SSH diagnostics cannot overwrite the active TUI.
pub(super) fn output_with_forwarded_stderr(
    mut child: Child,
    stdin: Option<&[u8]>,
) -> io::Result<Output> {
    let child_stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ssh command stderr missing"))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ssh command stdout missing"))?;
    // A ControlPersist master forked by this command may keep stderr open after the
    // command exits; the capture stops waiting for it shortly after the exit.
    let stdout_capture =
        PipeCapture::spawn_tail(child_stdout, SSH_STDOUT_CAPTURE_LIMIT, PipeEcho::None);
    let stderr_relay = PipeCapture::spawn(child_stderr, SSH_STDERR_CAPTURE_LIMIT, PipeEcho::Stderr);

    let write_result = if let Some(bytes) = stdin {
        if let Some(mut child_stdin) = child.stdin.take() {
            child_stdin.write_all(bytes)
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ssh bootstrap stdin missing",
            ))
        }
    } else {
        Ok(())
    };
    // OpenSSH points a daemonized master's stdin and stdout at /dev/null; only its
    // stderr handling has varied between releases. Reader threads drain both pipes
    // while retaining bounded output.
    let status_result = child.wait();
    let stdout_result = stdout_capture.finish(PIPE_DRAIN_GRACE);
    let stderr_result = stderr_relay.finish(PIPE_DRAIN_GRACE);

    let status = status_result?;
    write_result?;
    Ok(Output {
        status,
        stdout: stdout_result?,
        stderr: stderr_result?,
    })
}

pub(super) fn normalize_remote_output(mut output: Output) -> io::Result<Output> {
    normalize_remote_stdout(&mut output.stdout, output.status.success())?;
    Ok(output)
}

pub(super) fn normalize_remote_stdout(
    stdout: &mut Vec<u8>,
    command_succeeded: bool,
) -> io::Result<()> {
    let consumed = {
        let mut reader = io::Cursor::new(stdout.as_slice());
        match discard_remote_output_preamble(&mut reader) {
            Ok(()) => usize::try_from(reader.position()).unwrap_or(usize::MAX),
            Err(_) if !command_succeeded => return Ok(()),
            Err(err) => return Err(err),
        }
    };
    stdout.drain(..consumed.min(stdout.len()));
    Ok(())
}

pub(super) fn apply_noninteractive_ssh_options(command: &mut Command) {
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("NumberOfPasswordPrompts=0")
        .arg("-o")
        .arg("StrictHostKeyChecking=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-o")
        .arg("ConnectionAttempts=1")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=4");
}

pub(super) fn apply_managed_ssh_options(
    command: &mut Command,
    options: Option<&ManagedSshOptions>,
) {
    // Compress the first connection too: multiplexed bridges inherit the master's transport.
    command.arg("-C");
    let Some(options) = options else {
        return;
    };

    command.arg("-F").arg(&options.config_path);
    if let Some(control_path) = &options.control_path {
        // User ControlPaths may be shared across isolated Shepr configs (or
        // explicitly disabled). Managed auth must use our scoped transport;
        // never stop or unlink a master belonging to the user's SSH setup.
        command
            .arg("-S")
            .arg(control_path)
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg("ControlPersist=600");
    }
}

pub(super) fn ssh_config_quote(path: &str) -> String {
    format!("\"{path}\"")
}

/// Returns the quoted `Include` value for an SSH config file, or `None` when
/// there is no such file.
pub(super) fn ssh_config_include(path: Option<&Path>) -> Option<String> {
    path.filter(|path| path.is_file())
        .map(|path| ssh_config_quote(&path.to_string_lossy()))
}

/// The directory a managed config names the shared OpenSSH control socket
/// under.
///
/// Production uses the XDG runtime directory, checked private before any
/// socket is named under it, so an isolated environment never reaches a
/// user's live master. Tests that only render config text name a short
/// directory nothing binds in: OpenSSH's staging name leaves room only for a
/// directory as short as a real `/run/user/<uid>`, which no test scratch
/// directory is.
#[derive(Clone, Copy)]
pub(super) struct SshControlDir<'a> {
    path: &'a Path,
}

impl<'a> SshControlDir<'a> {
    /// The XDG runtime directory, refused unless it is private to this user.
    pub(super) fn runtime(app_paths: &'a shepr_config::AppPaths) -> io::Result<Self> {
        let path = app_paths.xdg_runtime_dir();
        shepr_platform::validate_ssh_runtime_dir(path)?;
        Ok(Self { path })
    }

    /// A directory taken as given, for tests that never bind the socket.
    #[cfg(test)]
    pub(super) fn unchecked(path: &'a Path) -> Self {
        Self { path }
    }
}

/// Builds a temporary ssh config that includes the user's settings first, so
/// OpenSSH's first-value-wins behavior preserves explicit user keepalives.
pub(super) fn write_managed_ssh_config(
    target: &str,
    app_paths: &shepr_config::AppPaths,
    control_dir: SshControlDir<'_>,
) -> io::Result<ManagedSshConfig> {
    let config_file = app_paths.config_file();
    let runtime_dir = app_paths.xdg_runtime_dir();
    let paths: shepr_platform::RemoteSshConfigPaths =
        shepr_platform::remote_ssh_config_paths(app_paths.home_dir());
    let control_path = Some(shepr_platform::ssh_control_path_under(
        control_dir.path,
        config_file,
        target,
    )?);

    let dir = shepr_platform::create_remote_ssh_config_dir(runtime_dir)?;
    let path = dir.join("config");
    let mut contents = String::new();
    for include in [
        ssh_config_include(paths.user_config.as_deref()),
        ssh_config_include(paths.system_config.as_deref()),
    ]
    .into_iter()
    .flatten()
    {
        contents.push_str(&format!("Include {include}\n"));
    }
    contents.push_str("Host *\n");
    contents.push_str("  ServerAliveInterval 15\n");
    contents.push_str("  ServerAliveCountMax 4\n");

    let write_result = (|| {
        let mut file = shepr_platform::create_private_file(&path)?;
        file.write_all(contents.as_bytes())
    })();
    if let Err(err) = write_result {
        let _ = fs::remove_dir_all(&dir);
        return Err(err);
    }
    Ok(ManagedSshConfig {
        options: ManagedSshOptions {
            config_path: path,
            control_path,
            _directory: Arc::new(ManagedSshConfigDirectory::new(dir)),
        },
    })
}

/// Preserve the SSH process exit code and its classified diagnostic in the error source.
pub(super) fn command_failed(context: &str, output: &Output) -> io::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let message = if stderr.is_empty() {
        format!("{context}: {}", output.status)
    } else {
        format!("{context}: {stderr}")
    };
    io::Error::other(super::SshFailureDiagnostic::from_ssh_output(
        output.status.code(),
        message,
    ))
}
