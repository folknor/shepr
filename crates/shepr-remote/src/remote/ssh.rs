use super::*;

use super::process::wait_with_output_timeout;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};

use crate::limits::SSH_COMMAND_TIMEOUT;

pub(super) mod ssh_options {
    use std::process::Command;

    pub(crate) const BATCH_MODE_NO: &str = "BatchMode=no";
    pub(crate) const BATCH_MODE_YES: &str = "BatchMode=yes";
    pub(crate) const CONTROL_MASTER: &str = "ControlMaster=auto";
    pub(crate) const STRICT_HOST_KEY_CHECKING: &str = "StrictHostKeyChecking=yes";
    pub(crate) const REMOTE_COMMAND_NONE: &str = "RemoteCommand=none";
    pub(crate) const LOG_LEVEL_ERROR: &str = "LogLevel=ERROR";

    /// Appends OpenSSH options using the same `-o` argument shape at each call site.
    pub(crate) fn append(command: &mut Command, options: &[&str]) {
        for option in options {
            command.arg("-o").arg(*option);
        }
    }

    /// Keep user config from replacing shepr's remote command or hiding SSH
    /// diagnostics, while still requiring an explicitly trusted host key.
    pub(crate) fn append_shepr_options(command: &mut Command) {
        append(
            command,
            &[
                STRICT_HOST_KEY_CHECKING,
                REMOTE_COMMAND_NONE,
                LOG_LEVEL_ERROR,
            ],
        );
    }

    impl crate::limits::SshKeepalive {
        pub(crate) fn append_config(&self, contents: &mut String) {
            contents.push_str(&self.config_lines());
        }

        pub(crate) fn config_lines(&self) -> String {
            format!(
                "  ServerAliveInterval {}\n  ServerAliveCountMax {}\n",
                self.interval_secs, self.count_max
            )
        }
    }
}

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
        remove_managed_config_directory(&self.path);
    }
}

/// Removes a temporary ssh config directory. Absent is already removed: the
/// exit sweep and an owner still dropping can both reach the same directory.
/// Any other failure leaves the directory in the runtime directory, which is
/// logged with its path since nothing retries it.
fn remove_managed_config_directory(path: &Path) {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            %error,
            path = %path.display(),
            "could not remove temporary ssh config directory"
        ),
    }
}

/// Files the SSH machinery leaves in the private profile runtime directory while it runs:
/// bridge sockets with their lock sidecars, and temporary ssh config
/// directories.
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
                // Absent or replaced by another owner's socket already count as
                // done inside `remove_socket_file_if_owned`.
                if let Err(error) = shepr_platform::ipc::remove_socket_file_if_owned(path, identity)
                {
                    tracing::warn!(
                        %error,
                        socket = %path.display(),
                        "could not remove ssh bridge socket at exit"
                    );
                }
                remove_bridge_socket_lock(path);
            }
            Self::Directory(path) => remove_managed_config_directory(path),
        }
    }
}

/// Tracks every live runtime-directory resource so the process can remove them
/// before it exits.
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

// The client has one process exit sweep, but bridge/config owners can still be
// unwinding on other threads when it runs. A per-endpoint registry would require
// the client supervisor to own and pass a tracker through every bridge and
// managed config, then sweep those trackers before process exit.
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
        // clock-io-ok: the grace covers owners dropping on other threads.
        let deadline = Instant::now() + grace;
        let mut pending = self.lock();
        while !pending.is_empty() {
            // clock-io-ok: condvar waits consume the grace period.
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
) -> io::Result<SshAuthenticationCommand> {
    let control_dir = SshControlDir::runtime(paths).map_err(|error| {
        crate::local_setup_error("could not prepare local SSH configuration", error)
    })?;
    let config =
        write_managed_ssh_config(target.as_str(), paths, control_dir).map_err(|error| {
            crate::local_setup_error("could not prepare local SSH configuration", error)
        })?;
    Ok(authentication_command_with_config(target, config))
}

pub(super) fn authentication_command_with_config(
    target: &SshTarget,
    config: ManagedSshConfig,
) -> SshAuthenticationCommand {
    let mut command = ssh_command();
    apply_managed_ssh_options(&mut command, Some(&config.options));
    command
        .env(shepr_core::env::ChildEnv::SshAskpassRequire, "never")
        .env_remove(shepr_core::env::ChildEnv::SshAskpass);
    ssh_options::append(
        &mut command,
        &[
            ssh_options::BATCH_MODE_NO,
            crate::limits::SSH_AUTHENTICATION_PASSWORD_PROMPTS_OPTION,
        ],
    );
    ssh_options::append_shepr_options(&mut command);
    command.arg("-T").arg(target.as_str()).arg("exit");
    SshAuthenticationCommand {
        command,
        _config: config,
    }
}

/// A configured machine's ssh, always in BatchMode: its commands do not open
/// authentication prompts. An agent can still wait for security-key presence
/// until the bounded command timeout; preflight then tries foreground SSH.
/// Interactive authentication goes through [`ssh_authentication_command`].
pub(crate) struct RemoteSsh {
    target: SshTarget,
    managed_config: ManagedSshConfig,
    /// Bounds commands launched by `sh_output` and `posix_user_shell_output`:
    /// each gets the shorter of its own timeout and the time left, and none
    /// starts once it has passed. A machine connection attempt sets it so
    /// discovery cannot outlast its budget.
    attempt_deadline: Option<Instant>,
}

impl RemoteSsh {
    /// For long-lived callers that already hold the launch-time config.
    pub(crate) fn new(target: SshTarget, paths: &shepr_config::AppPaths) -> io::Result<Self> {
        let control_dir = SshControlDir::runtime(paths).map_err(|error| {
            crate::local_setup_error("could not prepare local SSH configuration", error)
        })?;
        let managed_config = write_managed_ssh_config(target.as_str(), paths, control_dir)
            .map_err(|error| {
                crate::local_setup_error("could not prepare local SSH configuration", error)
            })?;
        Ok(Self {
            target,
            managed_config,
            attempt_deadline: None,
        })
    }

    pub(crate) fn set_attempt_deadline(&mut self, deadline: Option<Instant>) {
        self.attempt_deadline = deadline;
    }

    /// The timeout for the next command, or `TimedOut` when the attempt
    /// deadline has already passed and no further command may start.
    pub(super) fn command_timeout(&self, now: Instant) -> io::Result<CommandTimeout> {
        let Some(deadline) = self.attempt_deadline else {
            return Ok(CommandTimeout {
                duration: SSH_COMMAND_TIMEOUT,
                authentication_candidate: true,
            });
        };
        let remaining = deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            return Err(attempt_deadline_passed());
        }
        // A shorter command may be capped by time already spent in the attempt,
        // so its timeout alone is not enough evidence to offer foreground SSH.
        Ok(CommandTimeout {
            duration: remaining.min(SSH_COMMAND_TIMEOUT),
            authentication_candidate: remaining >= SSH_COMMAND_TIMEOUT,
        })
    }

    pub(super) fn target(&self) -> &str {
        self.target.as_str()
    }

    pub(crate) fn options(&self) -> &ManagedSshOptions {
        &self.managed_config.options
    }

    pub(super) fn command(&self) -> Command {
        let mut command = ssh_command();
        apply_managed_ssh_options(&mut command, Some(self.options()));
        apply_batch_ssh_options(&mut command);
        command.arg("-T").arg(self.target.as_str());
        command
    }

    pub(super) fn sh_output(&self, script: &str) -> io::Result<Output> {
        // clock-io-ok: earlier SSH round trips may have used the attempt budget.
        let timeout = self.command_timeout(Instant::now())?;
        self.sh_output_with_timeout(script, timeout.duration, timeout.authentication_candidate)
    }

    /// Runs `script` under `/bin/sh` on the remote host, giving the
    /// connection `timeout` instead of the round-trip budget. For a command that
    /// legitimately runs longer than one round trip, such as a server stop.
    pub(super) fn sh_output_within(&self, script: &str, timeout: Duration) -> io::Result<Output> {
        self.sh_output_with_timeout(script, timeout, false)
    }

    fn sh_output_with_timeout(
        &self,
        script: &str,
        timeout: Duration,
        authentication_candidate: bool,
    ) -> io::Result<Output> {
        let script = posix_remote_output_command(script);
        let mut child = self
            .command()
            .arg("/bin/sh -s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| crate::local_setup_error("could not start local ssh", error))?;

        let write_result = if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(script.as_bytes())
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ssh bootstrap stdin missing",
            ))
        };
        let output = wait_with_output_timeout(child, timeout)
            .map_err(|error| classify_command_timeout(error, authentication_candidate))?;
        finish_ssh_command(write_result, output)
    }

    /// Runs `remote_command` under `/bin/sh` through the remote user's login
    /// shell, so the command sees the user's `PATH`.
    pub(super) fn posix_user_shell_output(&self, remote_command: &str) -> io::Result<Output> {
        let mut command = self.command();
        command
            .arg(posix_remote_output_command(remote_command))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // clock-io-ok: earlier SSH round trips may have used the attempt budget.
        let timeout = self.command_timeout(Instant::now())?;
        let child = command
            .spawn()
            .map_err(|error| crate::local_setup_error("could not start local ssh", error))?;
        let output = wait_with_output_timeout(child, timeout.duration)
            .map_err(|error| classify_command_timeout(error, timeout.authentication_candidate))?;
        normalize_remote_output(output)
    }
}

#[derive(Debug)]
pub(super) struct CommandTimeout {
    duration: Duration,
    authentication_candidate: bool,
}

fn classify_command_timeout(error: io::Error, authentication_candidate: bool) -> io::Error {
    if error.kind() == io::ErrorKind::TimedOut && authentication_candidate {
        io::Error::new(
            error.kind(),
            super::SshFailureDiagnostic::authentication_wait_timeout(),
        )
    } else {
        error
    }
}

/// A failed ssh process has the diagnostic needed to classify authentication,
/// host-key and connection failures. Preserve it when writing the script also
/// failed because ssh closed stdin; only surface the write error if ssh itself
/// completed successfully.
fn finish_ssh_command(write_result: io::Result<()>, output: Output) -> io::Result<Output> {
    if !output.status.success() {
        return normalize_remote_output(output);
    }
    write_result.map_err(|error| {
        crate::local_setup_error("could not write the local SSH command", error)
    })?;
    normalize_remote_output(output)
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

pub(super) fn apply_batch_ssh_options(command: &mut Command) {
    ssh_options::append_shepr_options(command);
    ssh_options::append(
        command,
        &[
            ssh_options::BATCH_MODE_YES,
            crate::limits::SSH_NO_PASSWORD_PROMPTS_OPTION,
            crate::limits::SSH_CONNECT_TIMEOUT_OPTION,
            crate::limits::SSH_CONNECTION_ATTEMPTS_OPTION,
        ],
    );
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
        command.arg("-S").arg(control_path);
        ssh_options::append(
            command,
            &[
                ssh_options::CONTROL_MASTER,
                crate::limits::SSH_CONTROL_PERSIST_OPTION,
            ],
        );
    }
}

pub(super) fn ssh_config_quote(path: &str) -> String {
    format!("\"{path}\"")
}

/// Returns the quoted `Include` value for an SSH config file, or `None` when
/// there is no such file (or the path names something other than a file).
/// A stat failure other than absence, such as `EACCES`, is an error rather
/// than a silently dropped include.
pub(super) fn ssh_config_include(path: Option<&Path>) -> io::Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            tracing::debug!(path = %path.display(), "emitting SSH config include");
            Ok(Some(ssh_config_quote(&path.to_string_lossy())))
        }
        Ok(_) => {
            tracing::debug!(
                path = %path.display(),
                reason = "not_a_file",
                "skipping SSH config include"
            );
            Ok(None)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::debug!(
                path = %path.display(),
                reason = "not_found",
                "skipping SSH config include"
            );
            Ok(None)
        }
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("could not read SSH config {}: {error}", path.display()),
        )),
    }
}

/// An `ssh` child. Every one runs in `/`: shepr passes it only absolute paths,
/// and a ControlPersist master it forks outlives the command, so inheriting
/// shepr's working directory would pin that directory for the master's life.
pub(super) fn ssh_command() -> Command {
    shepr_platform::child_command("ssh", Path::new("/"))
}

/// The directory a managed config names the shared OpenSSH control socket
/// under.
///
/// Production uses the private profile runtime directory under the validated
/// XDG runtime root, so an isolated environment never reaches a user's live
/// master. Tests that only render config text name a short
/// directory nothing binds in: OpenSSH's staging name leaves room only for a
/// directory as short as a real `/run/user/<uid>`, which no test scratch
/// directory is.
#[derive(Clone, Copy)]
pub(super) struct SshControlDir<'a> {
    path: &'a Path,
}

impl<'a> SshControlDir<'a> {
    /// The private profile runtime directory under the XDG runtime root.
    pub(super) fn runtime(app_paths: &'a shepr_config::AppPaths) -> io::Result<Self> {
        let path = ensure_ssh_runtime_dir(app_paths)?;
        Ok(Self { path })
    }
}

/// Creates shepr's per-profile runtime directory below the validated XDG root,
/// then checks the resulting directory before SSH names sockets or config files
/// under it.
pub(super) fn ensure_ssh_runtime_dir(app_paths: &shepr_config::AppPaths) -> io::Result<&Path> {
    // Keep bridge sockets, SSH control sockets and managed configs under the
    // same validated XDG runtime root. A missing root returns its local setup
    // error, which the connector reports as Attention and retries; do not move
    // private SSH state to a fallback with a different lifetime or socket policy.
    shepr_platform::validate_ssh_runtime_dir(app_paths.xdg_runtime_dir())?;
    let runtime_dir = app_paths.runtime_dir();
    shepr_platform::create_private_directory_all(runtime_dir)?;
    shepr_platform::validate_ssh_runtime_dir(runtime_dir)?;
    Ok(runtime_dir)
}

/// Builds a temporary ssh config that includes the user's settings first, so
/// OpenSSH's first-value-wins behavior preserves explicit user keepalives.
pub(super) fn write_managed_ssh_config(
    target: &str,
    app_paths: &shepr_config::AppPaths,
    control_dir: SshControlDir<'_>,
) -> io::Result<ManagedSshConfig> {
    let config_file = app_paths.config_file();
    let runtime_dir = ensure_ssh_runtime_dir(app_paths)?;
    let paths: shepr_platform::RemoteSshConfigPaths =
        shepr_platform::remote_ssh_config_paths(app_paths.home_dir());
    let control_path = Some(shepr_platform::ssh_control_path_under(
        control_dir.path,
        config_file,
        target,
    )?);

    let dir =
        ManagedSshConfigDirectory::new(shepr_platform::create_remote_ssh_config_dir(runtime_dir)?);
    let path = dir.path.join("config");
    let mut contents = String::new();
    for include in [
        ssh_config_include(paths.user_config.as_deref())?,
        ssh_config_include(Some(paths.system_config.as_path()))?,
    ]
    .into_iter()
    .flatten()
    {
        contents.push_str(&format!("Include {include}\n"));
    }
    contents.push_str("Host *\n");
    crate::limits::SSH_KEEPALIVE.append_config(&mut contents);

    let mut file = shepr_platform::create_private_file(&path)?;
    file.write_all(contents.as_bytes())?;
    drop(file);
    Ok(ManagedSshConfig {
        options: ManagedSshOptions {
            config_path: path,
            control_path,
            _directory: Arc::new(dir),
        },
    })
}

/// Preserve the SSH process exit code and its classified diagnostic in the error source.
///
/// The remote stderr is kept whole (control characters stripped, size bounded
/// by the capture): it is the diagnostic, and the operator who reads it owns
/// both hosts, so a login banner or hostname in it exposes nothing new.
/// `ssh_bridge_exit_error` makes the same choice.
pub(super) fn command_failed(context: &str, output: &Output) -> io::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = super::server_lifecycle::printable_remote_text(stderr.trim());
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

#[cfg(test)]
impl RemoteSsh {
    pub(super) fn test_with_state(target: SshTarget, managed_config: ManagedSshConfig) -> Self {
        Self {
            target,
            managed_config,
            attempt_deadline: None,
        }
    }
}

#[cfg(test)]
impl<'a> SshControlDir<'a> {
    /// A directory taken as given, for tests that never bind the socket.
    pub(super) fn unchecked(path: &'a Path) -> Self {
        Self { path }
    }
}

#[cfg(test)]
#[path = "ssh_tests.rs"]
mod ssh_tests;
