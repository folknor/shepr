use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, atomic::Ordering};
use std::time::{Duration, Instant};

use crate::bridge::discard_remote_output_preamble;
use crate::failure::{
    SshFailureDiagnostic, attempt_deadline_passed, local_setup_error, ssh_runtime_error,
};
use crate::limits::{SSH_COMMAND_TIMEOUT, SSH_KEEPALIVE};
use crate::machine::SshTarget;
use crate::process::wait_with_output_timeout;
use crate::shell_command::{AccountShellCommand, PosixScript, posix_remote_output_command};
use crate::ssh_paths::{
    RemoteSshConfigPaths, SshControlKey, create_remote_ssh_config_dir,
    release_remote_ssh_config_dir, remote_ssh_config_file_path, remote_ssh_config_paths,
    ssh_control_path_under, validate_ssh_runtime_dir,
};

mod ssh_options {
    use std::process::Command;

    pub(super) const BATCH_MODE_NO: &str = "BatchMode=no";
    pub(super) const BATCH_MODE_YES: &str = "BatchMode=yes";
    pub(super) const CONTROL_MASTER: &str = "ControlMaster=auto";
    pub(super) const STRICT_HOST_KEY_CHECKING: &str = "StrictHostKeyChecking=yes";
    pub(super) const REMOTE_COMMAND_NONE: &str = "RemoteCommand=none";
    pub(super) const LOG_LEVEL_ERROR: &str = "LogLevel=ERROR";

    /// Appends OpenSSH options using the same `-o` argument shape at each call site.
    pub(super) fn append(command: &mut Command, options: &[&str]) {
        for option in options {
            command.arg("-o").arg(*option);
        }
    }

    /// Keep user config from replacing shepr's remote command or hiding SSH
    /// diagnostics, while still requiring an explicitly trusted host key.
    pub(super) fn append_shepr_options(command: &mut Command) {
        append(
            command,
            &[
                STRICT_HOST_KEY_CHECKING,
                REMOTE_COMMAND_NONE,
                LOG_LEVEL_ERROR,
            ],
        );
    }

    /// Bound connection establishment consistently for foreground and
    /// non-interactive SSH commands.
    pub(super) fn append_connection_bounds(command: &mut Command) {
        append(
            command,
            &[
                crate::limits::SSH_CONNECT_TIMEOUT_OPTION,
                crate::limits::SSH_CONNECTION_ATTEMPTS_OPTION,
            ],
        );
    }
}

#[derive(Clone)]
pub(crate) struct ManagedSshOptions {
    pub(crate) config_path: PathBuf,
    control_path: Option<PathBuf>,
    // Bridge workers may launch SSH after the helper that created this config
    // has gone away. The last options owner removes only the temporary config.
    _directory: Arc<ManagedSshConfigDirectory>,
}

struct ManagedSshConfig {
    options: ManagedSshOptions,
}

struct ManagedSshConfigDirectory {
    path: PathBuf,
    // Declared after `path` and dropped after `Drop::drop` has removed the directory,
    // so the exit sweep only ever sees directories that still exist.
    _teardown: TeardownRegistration,
}

impl ManagedSshConfigDirectory {
    fn new(path: PathBuf) -> Self {
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
/// logged with its path; dead-owner sweeps retry conservative cleanup.
fn remove_managed_config_directory(path: &Path) {
    release_remote_ssh_config_dir(path);
}

/// Temporary SSH config directories awaiting owner or process-exit cleanup.
pub(crate) enum TeardownResource {
    Directory(PathBuf),
}

impl TeardownResource {
    fn remove(&self) {
        match self {
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
/// exit does not wait for them, which leaked config directories.
pub(crate) struct TeardownRegistry {
    pending: std::sync::Mutex<Vec<(u64, TeardownResource)>>,
    changed: std::sync::Condvar,
    next_id: std::sync::atomic::AtomicU64,
}

// The client has one process exit sweep, but bridge/config owners can still be
// unwinding on other threads when it runs. A per-endpoint registry would require
// the client supervisor to own and pass a tracker through every bridge and
// managed config, then sweep those trackers before process exit.
pub(crate) static SSH_TEARDOWN: TeardownRegistry = TeardownRegistry::new();

impl TeardownRegistry {
    const fn new() -> Self {
        Self {
            pending: std::sync::Mutex::new(Vec::new()),
            changed: std::sync::Condvar::new(),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(u64, TeardownResource)>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn register(&'static self, resource: TeardownResource) -> TeardownRegistration {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock().push((id, resource));
        TeardownRegistration { registry: self, id }
    }

    /// Gives owners that are already dropping up to `grace` to finish, then
    /// removes whatever is still registered.
    fn release_all(&self, grace: Duration) {
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

pub(crate) struct TeardownRegistration {
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

/// Removes the temporary SSH config directories this process
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
    paths: &shepr_paths::AppPaths,
    target: &SshTarget,
) -> io::Result<SshAuthenticationCommand> {
    let control_dir = SshControlDir::runtime(paths)
        .map_err(|error| local_setup_error("could not prepare local SSH configuration", error))?;
    let config = write_managed_ssh_config(target, paths, control_dir)
        .map_err(|error| local_setup_error("could not prepare local SSH configuration", error))?;
    Ok(authentication_command_with_config(target, config))
}

fn authentication_command_with_config(
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
    ssh_options::append_connection_bounds(&mut command);
    ssh_options::append_shepr_options(&mut command);
    command.arg("-T");
    target.append_to(&mut command);
    command.arg("exit");
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
    /// Bounds commands launched by `sh_output`:
    /// each gets the shorter of its own timeout and the time left, and none
    /// starts once it has passed. A machine connection attempt sets it so
    /// discovery cannot outlast its budget.
    attempt_deadline: Option<Instant>,
}

impl RemoteSsh {
    /// For long-lived callers that already hold the launch-time config.
    pub(crate) fn new(target: SshTarget, paths: &shepr_paths::AppPaths) -> io::Result<Self> {
        let control_dir = SshControlDir::runtime(paths).map_err(|error| {
            local_setup_error("could not prepare local SSH configuration", error)
        })?;
        let managed_config =
            write_managed_ssh_config(&target, paths, control_dir).map_err(|error| {
                local_setup_error("could not prepare local SSH configuration", error)
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
    fn command_timeout(&self, now: Instant) -> io::Result<CommandTimeout> {
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

    pub(crate) fn target(&self) -> &SshTarget {
        &self.target
    }

    pub(crate) fn options(&self) -> &ManagedSshOptions {
        &self.managed_config.options
    }

    fn command(&self) -> Command {
        let mut command = ssh_command();
        apply_managed_ssh_options(&mut command, Some(self.options()));
        apply_batch_ssh_options(&mut command);
        command.arg("-T");
        self.target.append_to(&mut command);
        command
    }

    /// Runs `script` under `/bin/sh` on the remote host. sshd hands the account
    /// shell only `/bin/sh -s`, a plain external command any shell accepts, and
    /// the script travels on stdin for `/bin/sh` to parse. `/bin/sh` inherits
    /// the environment the account shell's non-login startup exported, so a
    /// `command -v` here sees that shell's PATH.
    pub(crate) fn sh_output(&self, script: &PosixScript) -> io::Result<Output> {
        // clock-io-ok: earlier SSH round trips may have used the attempt budget.
        let timeout = self.command_timeout(Instant::now())?;
        self.sh_output_with_timeout(script, timeout.duration, timeout.authentication_candidate)
    }

    /// Runs `script` under `/bin/sh` on the remote host, giving the
    /// connection `timeout` instead of the round-trip budget. For a command that
    /// legitimately runs longer than one round trip, such as a server stop.
    pub(crate) fn sh_output_within(
        &self,
        script: &PosixScript,
        timeout: Duration,
    ) -> io::Result<Output> {
        self.sh_output_with_timeout(script, timeout, false)
    }

    fn sh_output_with_timeout(
        &self,
        script: &PosixScript,
        timeout: Duration,
        authentication_candidate: bool,
    ) -> io::Result<Output> {
        let script = posix_remote_output_command(script);
        let mut child = self
            .command()
            .arg(AccountShellCommand::posix_script_stdin().as_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| local_setup_error("could not start local ssh", error))?;

        let write_result = if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(script.as_str().as_bytes())
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
}

#[derive(Debug)]
struct CommandTimeout {
    duration: Duration,
    authentication_candidate: bool,
}

fn classify_command_timeout(error: io::Error, authentication_candidate: bool) -> io::Error {
    if error.kind() == io::ErrorKind::TimedOut && authentication_candidate {
        io::Error::new(
            error.kind(),
            SshFailureDiagnostic::authentication_wait_timeout(),
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
    write_result
        .map_err(|error| local_setup_error("could not write the local SSH command", error))?;
    normalize_remote_output(output)
}

fn normalize_remote_output(mut output: Output) -> io::Result<Output> {
    normalize_remote_stdout(&mut output.stdout, output.status.success())?;
    Ok(output)
}

pub(crate) fn normalize_remote_stdout(
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

pub(crate) fn apply_batch_ssh_options(command: &mut Command) {
    ssh_options::append_shepr_options(command);
    ssh_options::append(
        command,
        &[
            ssh_options::BATCH_MODE_YES,
            crate::limits::SSH_NO_PASSWORD_PROMPTS_OPTION,
        ],
    );
    ssh_options::append_connection_bounds(command);
}

pub(crate) fn apply_managed_ssh_options(
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

fn ssh_config_quote(path: &Path) -> io::Result<String> {
    let Some(path_text) = path.to_str() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("SSH config path {path:?} is not valid UTF-8"),
        ));
    };
    if path_text.contains('"') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("SSH config path {path:?} contains a double quote"),
        ));
    }
    Ok(format!("\"{path_text}\""))
}

/// Returns the quoted `Include` value for an SSH config file, or `None` when
/// there is no such file (or the path names something other than a file).
/// A stat failure other than absence, such as `EACCES`, is an error rather
/// than a silently dropped include, and so is a file whose path the `Include`
/// line cannot spell (not UTF-8, or holding a double quote).
fn ssh_config_include(path: Option<&Path>) -> io::Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            let include = ssh_config_quote(path)?;
            tracing::debug!(path = %path.display(), "emitting SSH config include");
            Ok(Some(include))
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
pub(crate) fn ssh_command() -> Command {
    shepr_platform::child_command("ssh", Path::new("/"))
}

/// The directory a managed config names the shared OpenSSH control socket
/// under.
///
/// Production uses the private profile runtime directory under the validated
/// XDG runtime root, so an isolated environment never reaches a user's live
/// master. Construction requires the profile runtime directory to pass the
/// ownership, permission and path checks.
#[derive(Clone, Copy)]
struct SshControlDir<'a> {
    path: &'a Path,
}

impl<'a> SshControlDir<'a> {
    /// The private profile runtime directory under the XDG runtime root.
    fn runtime(app_paths: &'a shepr_paths::AppPaths) -> io::Result<Self> {
        let path = ensure_ssh_runtime_dir(app_paths)?;
        Ok(Self { path })
    }
}

/// Creates shepr's per-profile runtime directory below the validated XDG root,
/// then checks the resulting directory before SSH names sockets or config files
/// under it.
pub(crate) fn ensure_ssh_runtime_dir(app_paths: &shepr_paths::AppPaths) -> io::Result<&Path> {
    // Keep SSH control sockets and managed configs under the
    // same validated XDG runtime root. A missing root returns its local setup
    // error, which the connector reports as Attention and retries; do not move
    // private SSH state to a fallback with a different lifetime or socket policy.
    validate_ssh_runtime_dir(app_paths.xdg_runtime_dir()).map_err(ssh_runtime_error)?;
    let runtime_dir = app_paths.runtime_dir();
    shepr_platform::create_private_runtime_directory(runtime_dir)?;
    validate_ssh_runtime_dir(runtime_dir).map_err(ssh_runtime_error)?;
    Ok(runtime_dir)
}

/// Builds a temporary ssh config that includes the user's settings first, so
/// OpenSSH's first-value-wins behavior preserves explicit user keepalives.
fn write_managed_ssh_config(
    target: &SshTarget,
    app_paths: &shepr_paths::AppPaths,
    control_dir: SshControlDir<'_>,
) -> io::Result<ManagedSshConfig> {
    let config_file = app_paths.client_config_file();
    let runtime_dir = control_dir.path;
    let paths: RemoteSshConfigPaths = remote_ssh_config_paths(app_paths.home_dir());
    let control_path = Some(ssh_control_path_under(
        control_dir.path,
        &config_file,
        SshControlKey::for_target(target),
    )?);

    write_managed_ssh_config_at(runtime_dir, &paths, control_path)
}

fn write_managed_ssh_config_at(
    runtime_dir: &Path,
    paths: &RemoteSshConfigPaths,
    control_path: Option<PathBuf>,
) -> io::Result<ManagedSshConfig> {
    let dir = ManagedSshConfigDirectory::new(
        create_remote_ssh_config_dir(runtime_dir).map_err(ssh_runtime_error)?,
    );
    let path = remote_ssh_config_file_path(&dir.path);
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
    contents.push_str(&SSH_KEEPALIVE.config_lines());

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
/// The captured remote stderr becomes the diagnostic. `SshFailureDiagnostic`
/// wraps it in `RemoteText` once, so line breaks remain readable and terminal
/// control characters cannot affect local output. `ssh_bridge_exit_error` uses
/// the same boundary.
pub(crate) fn command_failed(context: &str, output: &Output) -> io::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let message = if stderr.is_empty() {
        format!("{context}: {}", output.status)
    } else {
        format!("{context}: {stderr}")
    };
    io::Error::other(SshFailureDiagnostic::from_ssh_output(
        output.status.code(),
        &message,
    ))
}

/// The command an operator runs to check SSH access to `target` by hand, with
/// the target quoted for a POSIX shell.
pub fn ssh_check_command(target: &SshTarget) -> String {
    format!("ssh {}", target.shell_word())
}

#[cfg(test)]
impl RemoteSsh {
    fn test_with_state(target: SshTarget, managed_config: ManagedSshConfig) -> Self {
        Self {
            target,
            managed_config,
            attempt_deadline: None,
        }
    }
}

#[cfg(test)]
mod tests;
