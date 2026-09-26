//! Remote thin-client launcher over SSH command stdio.

use super::{
    args::*,
    process::{PIPE_DRAIN_GRACE, PipeCapture, PipeEcho, wait_with_output_timeout},
    restart_policy::*,
    shell_quote,
};
use std::fs;
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use interprocess::TryClone as _;
use interprocess::local_socket::ListenerNonblockingMode;
use interprocess::local_socket::traits::Listener as _;
#[cfg(test)]
use interprocess::local_socket::traits::Stream as _;
use serde::Deserialize;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const BRIDGE_ACCEPT_POLL: Duration = Duration::from_millis(50);
const BRIDGE_IO_POLL: Duration = Duration::from_millis(1);
const BRIDGE_SOCKET_PERMISSION_MODE: u32 = 0o600;
const REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);
const NONINTERACTIVE_SSH_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const NONINTERACTIVE_SSH_STDERR_LIMIT: usize = 16 * 1024;
const BRIDGE_FAILURE_REPORT_TIMEOUT: Duration = Duration::from_secs(1);
const REMOTE_SERVER_SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);
const REMOTE_OUTPUT_READY_MARKER: &str = "shepr-remote-output-ready:1";
const SSH_CONTROL_SOCKET_NAME: &str = "ctl";
pub(crate) fn run_remote(remote: RemoteLaunch) -> io::Result<()> {
    let session_name = crate::session::active_name()
        .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string());
    let local_socket = local_forward_socket_path(&remote.target, &session_name);
    let program = std::env::args()
        .next()
        .unwrap_or_else(|| "shepr".to_string());
    let reattach_command =
        reattach_command(&program, &remote.target, &session_name, remote.keybindings);
    let manage_ssh_config = crate::config::Config::load()
        .config
        .remote
        .manage_ssh_config;
    let require_surface_interest = crate::client::endpoint::EndpointCatalog::load()
        .map(|catalog| catalog.contains_enabled_target_session(&remote.target, &session_name))
        .unwrap_or(false);
    let remote_ssh = RemoteSsh::new(
        remote.target.clone(),
        manage_ssh_config,
        session_name.clone(),
    );
    let prepared_remote = prepare_remote_shepr(&remote_ssh, require_surface_interest)?;
    ensure_remote_server_ready(
        &remote_ssh,
        &prepared_remote.remote_shepr,
        require_surface_interest,
    )?;

    let _bridge = SshStdioBridge::start(
        remote.target,
        &prepared_remote.remote_shepr,
        local_socket.clone(),
        &session_name,
        remote_ssh.options(),
        false,
    )?;

    run_client_process(&local_socket, &reattach_command, remote.keybindings)
}

pub(crate) fn check_saved_ssh(target: &str, session: &str) -> io::Result<()> {
    super::validate_remote_target(target)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    crate::session::validate_name(session)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut ssh = RemoteSsh::new_noninteractive(target.to_owned());
    ssh.session_name = session.to_owned();
    let remote = find_installed_remote_shepr(&ssh)?;
    match remote_server_status(&ssh, &remote, false)? {
        RemoteServerStatus::Running {
            protocol,
            surface_interest,
            health_check,
            detached_server_daemon,
            ..
        } if remote_server_restart_reason(
            protocol,
            detached_server_daemon,
            true,
            surface_interest,
            health_check,
        )
        .is_none() =>
        {
            Ok(())
        }
        _ => Err(io::Error::other(format!(
            "remote Shepr server is stopped or incompatible; run `{}`",
            super::saved_ssh_bootstrap_command(target, session),
        ))),
    }
}

pub(crate) fn prepare_saved_ssh(
    target: &str,
    session_name: &str,
) -> io::Result<Option<crate::client::endpoint::SshMachineMetadata>> {
    super::validate_remote_target(target)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    crate::session::validate_name(session_name)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let manage_ssh_config = crate::config::Config::load()
        .config
        .remote
        .manage_ssh_config;
    let ssh = RemoteSsh::new(
        target.to_owned(),
        manage_ssh_config,
        session_name.to_owned(),
    );
    let prepared = prepare_remote_shepr(&ssh, true)?;
    ensure_remote_server_ready(&ssh, &prepared.remote_shepr, true)?;

    // The bridge already owns daemon startup. EOF closes only this temporary attachment,
    // leaving the named server running even when no local TUI is open yet.
    let command = prepared.remote_shepr.saved_bridge_command(session_name);
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server startup failed", &output));
    }
    match remote_server_status(&ssh, &prepared.remote_shepr, true)? {
        RemoteServerStatus::Running {
            protocol,
            surface_interest,
            health_check,
            detached_server_daemon,
            ..
        } if remote_server_restart_reason(
            protocol,
            detached_server_daemon,
            true,
            surface_interest,
            health_check,
        )
        .is_none() =>
        {
            Ok(prepared.remote_shepr.machine_metadata().or_else(|| {
                discover_remote_api_metadata(&ssh, session_name)
                    .inspect_err(
                        |error| tracing::debug!(%error, "could not capture SSH setup metadata"),
                    )
                    .ok()
            }))
        }
        _ => Err(io::Error::other(
            "remote server is not ready for saved machines",
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemoteShepr {
    path: String,
}

impl RemoteShepr {
    fn new(path: &str) -> Self {
        Self {
            path: path.to_owned(),
        }
    }

    /// The executable recorded by an earlier successful discovery. It is only a hint:
    /// the endpoint handshake still checks the protocol version it speaks.
    pub(super) fn from_metadata(
        metadata: &crate::client::endpoint::SshMachineMetadata,
    ) -> Option<Self> {
        metadata.is_valid().then(|| Self::new(&metadata.executable))
    }

    pub(super) fn machine_metadata(&self) -> Option<crate::client::endpoint::SshMachineMetadata> {
        let metadata = crate::client::endpoint::SshMachineMetadata {
            os: "linux".to_owned(),
            executable: self.path.clone(),
        };
        metadata.is_valid().then_some(metadata)
    }

    fn quoted(&self) -> String {
        shell_quote(&self.path)
    }

    fn command(&self, args: &[&str]) -> String {
        let mut command = self.quoted();
        for arg in args {
            command.push(' ');
            command.push_str(&shell_quote(arg));
        }
        command
    }

    fn session_command(&self, session_name: &str, args: &[&str]) -> String {
        self.command(&Self::session_args(session_name, args))
    }

    fn session_args<'a>(session_name: &'a str, args: &[&'a str]) -> Vec<&'a str> {
        let mut session_args = Vec::with_capacity(args.len() + 2);
        if session_name != crate::session::DEFAULT_SESSION_NAME {
            session_args.extend(["--session", session_name]);
        }
        session_args.extend_from_slice(args);
        session_args
    }

    fn status_client_command(&self) -> String {
        format!(
            "test -x {} && {}",
            self.quoted(),
            self.command(&["status", "client", "--json"])
        )
    }

    fn bridge_command(&self, session_name: &str, idle_timeout: bool) -> String {
        let command = if idle_timeout {
            &["remote-client-bridge", "--idle-timeout-v1"][..]
        } else {
            &["remote-client-bridge"][..]
        };
        let args = Self::session_args(session_name, command);
        // sshd hands this string to the user's login shell, which need not be POSIX
        // (xonsh, fish, nushell). Run the script under /bin/sh, as discovery and the
        // API bridge do, so the login shell only has to launch one quoted command.
        posix_shell_command(&posix_remote_output_command(&format!(
            "exec {}",
            self.command(&args)
        )))
    }

    fn saved_bridge_command(&self, session_name: &str) -> String {
        let args = Self::session_args(session_name, &["remote-client-bridge"]);
        format!("exec {} </dev/null", self.command(&args))
    }
}

/// Prefixes `command` with the output-ready marker line (preceded by a newline, so
/// the marker starts a line of its own after any login banner).
///
/// The prefix is deliberately plain words with no quotes or newlines. For a plain
/// `command` such as the client bridge's `exec <path> ...`, the wrapped result of
/// [`posix_shell_command`] reaches a non-POSIX login shell as `/bin/sh -c` plus one
/// single-quoted argument with nothing inside it to escape.
fn posix_remote_output_command(command: &str) -> String {
    format!("echo; echo {REMOTE_OUTPUT_READY_MARKER}; {command}")
}

/// Runs a POSIX script under `/bin/sh` regardless of the remote login shell.
fn posix_shell_command(script: &str) -> String {
    format!("/bin/sh -c {}", shell_quote(script))
}

pub(super) struct PreparedRemoteShepr {
    pub(super) remote_shepr: RemoteShepr,
}

#[derive(Clone)]
pub(super) struct ManagedSshOptions {
    config_path: PathBuf,
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
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Files the SSH machinery leaves in the temp directory while it runs: bridge
/// sockets and temporary ssh config directories.
enum TeardownResource {
    Socket {
        path: PathBuf,
        identity: crate::ipc::SocketFileIdentity,
    },
    Directory(PathBuf),
}

impl TeardownResource {
    fn remove(&self) {
        match self {
            Self::Socket { path, identity } => {
                let _ = crate::ipc::remove_socket_file_if_owned(path, identity);
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
struct TeardownRegistry {
    pending: std::sync::Mutex<Vec<(u64, TeardownResource)>>,
    changed: std::sync::Condvar,
    next_id: std::sync::atomic::AtomicU64,
}

static SSH_TEARDOWN: TeardownRegistry = TeardownRegistry::new();

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

    fn register(&'static self, resource: TeardownResource) -> TeardownRegistration {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock().push((id, resource));
        TeardownRegistration { registry: self, id }
    }

    /// Gives owners that are already dropping up to `grace` to finish, then
    /// removes whatever is still registered.
    fn release_all(&self, grace: Duration) {
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

struct TeardownRegistration {
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
pub(crate) fn release_ssh_resources_before_exit(grace: Duration) {
    SSH_TEARDOWN.release_all(grace);
}

/// Classify only SSH authentication diagnostics, not transport failures or
/// unknown/changed host keys. This does not imply permission to prompt.
pub(crate) fn ssh_error_requires_authentication(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    if message.contains("host key verification failed")
        || message.contains("remote host identification has changed")
    {
        return false;
    }
    (message.contains("permission denied")
        && ["(publickey", "(keyboard-interactive", "(password"]
            .iter()
            .any(|method| message.contains(method)))
        || (message.contains("signing failed")
            && (message.contains("sign_and_send_pubkey") || message.contains("agent")))
}

/// Keep this owner alive until the child has exited: OpenSSH reads its temporary
/// config after spawn. Dropping it never stops the shared authenticated master.
pub(crate) struct SshAuthenticationCommand {
    pub(crate) command: Command,
    _config: ManagedSshConfig,
}

pub(crate) fn ssh_authentication_command(target: &str) -> io::Result<SshAuthenticationCommand> {
    if target.is_empty() || target.starts_with('-') || target.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid SSH target",
        ));
    }
    if !crate::config::Config::load()
        .config
        .remote
        .manage_ssh_config
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interactive SSH recovery requires remote.manage_ssh_config=true",
        ));
    }
    let config = write_managed_ssh_config(target)?;
    Ok(authentication_command_with_config(target, config))
}

fn authentication_command_with_config(
    target: &str,
    config: ManagedSshConfig,
) -> SshAuthenticationCommand {
    let mut command = Command::new("ssh");
    apply_managed_ssh_options(&mut command, Some(&config.options));
    command
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env_remove("SSH_ASKPASS")
        .arg("-o")
        .arg("BatchMode=no")
        .arg("-o")
        .arg("StrictHostKeyChecking=yes")
        .arg("-o")
        .arg("NumberOfPasswordPrompts=3")
        .arg("-T")
        .arg(target)
        .arg("exit");
    SshAuthenticationCommand {
        command,
        _config: config,
    }
}

pub(super) struct RemoteSsh {
    target: String,
    session_name: String,
    managed_config: Option<ManagedSshConfig>,
    noninteractive: bool,
}

impl RemoteSsh {
    fn new(target: String, manage_ssh_config: bool, session_name: String) -> Self {
        let managed_config = if manage_ssh_config {
            write_managed_ssh_config(&target)
                .inspect_err(|err| {
                    tracing::debug!(%err, "could not write managed ssh config; using plain ssh");
                })
                .ok()
        } else {
            None
        };

        Self {
            target,
            session_name,
            managed_config,
            noninteractive: false,
        }
    }

    /// For one-shot CLI commands, which read the config at their own launch.
    pub(super) fn new_noninteractive(target: String) -> Self {
        let manage = crate::config::Config::load()
            .config
            .remote
            .manage_ssh_config;
        Self::new_noninteractive_with(target, manage)
    }

    /// For long-lived callers that already hold the launch-time config.
    pub(super) fn new_noninteractive_with(target: String, manage_ssh_config: bool) -> Self {
        let mut ssh = Self::new(
            target,
            manage_ssh_config,
            crate::session::DEFAULT_SESSION_NAME.into(),
        );
        ssh.noninteractive = true;
        ssh
    }

    /// Whether this was built to use a managed ssh config but writing it failed.
    pub(super) fn missing_managed_config(&self, manage_ssh_config: bool) -> bool {
        manage_ssh_config && self.managed_config.is_none()
    }

    fn target(&self) -> &str {
        &self.target
    }

    fn destination(&self) -> String {
        format!("{} (session {})", self.target, self.session_name)
    }

    pub(super) fn options(&self) -> Option<&ManagedSshOptions> {
        self.managed_config.as_ref().map(|config| &config.options)
    }

    fn command(&self) -> Command {
        let mut command = self.base_command();
        if self.noninteractive {
            apply_noninteractive_ssh_options(&mut command);
        }
        command.arg("-T").arg(&self.target);
        command
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new("ssh");
        apply_managed_ssh_options(&mut command, self.options());
        command
    }

    fn sh_output(&self, script: &str) -> io::Result<Output> {
        let script = posix_remote_output_command(script);
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
        let output = wait_with_output_timeout(child, NONINTERACTIVE_SSH_COMMAND_TIMEOUT)?;
        write_result?;
        normalize_remote_output(output)
    }

    fn framed_user_shell_output(&self, remote_command: &str) -> io::Result<Output> {
        let mut command = self.command();
        command
            .arg(remote_command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = if self.noninteractive {
            wait_with_output_timeout(command.spawn()?, NONINTERACTIVE_SSH_COMMAND_TIMEOUT)
        } else {
            output_with_forwarded_stderr(command.spawn()?, None)
        }?;
        normalize_remote_output(output)
    }

    fn posix_user_shell_output(&self, remote_command: &str) -> io::Result<Output> {
        self.framed_user_shell_output(&posix_remote_output_command(remote_command))
    }
}

// Only interactive setup uses this relay. Background probes retain their
// capture-only timeout path so SSH diagnostics cannot overwrite the active TUI.
fn output_with_forwarded_stderr(mut child: Child, stdin: Option<&[u8]>) -> io::Result<Output> {
    let child_stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ssh command stderr missing"))?;
    // A ControlPersist master forked by this command may keep stderr open after the
    // command exits; the capture stops waiting for it shortly after the exit.
    let stderr_relay = PipeCapture::spawn(child_stderr, usize::MAX, PipeEcho::Stderr);

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
    // Stdout is read to its end: OpenSSH points a daemonized master's stdin and stdout
    // at /dev/null; only its stderr handling has varied between releases.
    let output_result = child.wait_with_output();
    let stderr_result = stderr_relay.finish(PIPE_DRAIN_GRACE);

    let mut output = output_result?;
    write_result?;
    output.stderr = stderr_result?;
    Ok(output)
}

fn normalize_remote_output(mut output: Output) -> io::Result<Output> {
    normalize_remote_stdout(&mut output.stdout, output.status.success())?;
    Ok(output)
}

fn normalize_remote_stdout(stdout: &mut Vec<u8>, command_succeeded: bool) -> io::Result<()> {
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

fn apply_noninteractive_ssh_options(command: &mut Command) {
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

fn apply_managed_ssh_options(command: &mut Command, options: Option<&ManagedSshOptions>) {
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

fn locate_remote_shepr(ssh: &RemoteSsh, require_surface_interest: bool) -> io::Result<RemoteShepr> {
    let candidates = remote_binary_candidates(ssh)?;
    for candidate in candidates {
        if let Some(status) = remote_client_status(ssh, &candidate)?
            && status.supports_endpoint_requirement(require_surface_interest)
        {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "matching Shepr is not ready on {}; install or update it there manually and retry",
            ssh.target(),
        ),
    ))
}

pub(super) fn prepare_remote_shepr(
    ssh: &RemoteSsh,
    require_surface_interest: bool,
) -> io::Result<PreparedRemoteShepr> {
    Ok(PreparedRemoteShepr {
        remote_shepr: locate_remote_shepr(ssh, require_surface_interest)?,
    })
}

pub(super) fn find_installed_remote_shepr(ssh: &RemoteSsh) -> io::Result<RemoteShepr> {
    locate_remote_shepr(ssh, true)
}

pub(super) fn discover_remote_api_metadata(
    ssh: &RemoteSsh,
    session: &str,
) -> io::Result<crate::client::endpoint::SshMachineMetadata> {
    let output = ssh.framed_user_shell_output(&posix_remote_api_discovery_command(session))?;
    if !output.status.success() {
        return Err(command_failed("remote binary discovery failed", &output));
    }
    let metadata = crate::client::endpoint::SshMachineMetadata {
        os: "linux".to_owned(),
        executable: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    };
    if !metadata.is_valid() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid remote Shepr executable path",
        ));
    }
    Ok(metadata)
}

fn remote_binary_candidates(ssh: &RemoteSsh) -> io::Result<Vec<RemoteShepr>> {
    let mut candidates = Vec::new();

    if let Some(path_candidate) = remote_binary_on_path_any(ssh)? {
        push_if_new_remote_binary_candidate(&mut candidates, path_candidate);
    }

    let output = ssh.sh_output(&known_remote_binary_candidate_script())?;
    if !output.status.success() {
        return Err(command_failed("remote binary discovery failed", &output));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for candidate in remote_sheprs_from_path_discovery(&stdout) {
        push_if_new_remote_binary_candidate(&mut candidates, candidate);
    }

    Ok(candidates)
}

fn push_if_new_remote_binary_candidate(candidates: &mut Vec<RemoteShepr>, candidate: RemoteShepr) {
    if !candidates
        .iter()
        .any(|existing| existing.path == candidate.path)
    {
        candidates.push(candidate);
    }
}

/// Install locations checked before falling back to `command -v shepr`, which
/// misses these when a non-interactive SSH shell has a minimal PATH.
fn known_remote_binary_candidate_script() -> String {
    String::from(
        r#"home=${HOME:-}
emit() {
    path=$1
    if [ -n "$path" ] && [ -x "$path" ]; then
        printf '%s\n' "$path"
    fi
}
if [ -n "$home" ]; then
    emit "$home/.cargo/bin/shepr"
    emit "$home/.local/bin/shepr"
fi
"#,
    )
}

fn remote_binary_on_path_any(ssh: &RemoteSsh) -> io::Result<Option<RemoteShepr>> {
    let output = ssh.posix_user_shell_output("command -v shepr")?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(candidate) = remote_shepr_from_path_discovery(&stdout) {
            return Ok(Some(candidate));
        }
    }

    // Non-POSIX login shells such as xonsh reject `command -v`; retry through
    // /bin/sh while retaining the login-shell probe for shell-initialized PATHs.
    let output = ssh.sh_output("command -v shepr\n")?;
    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(remote_shepr_from_path_discovery(&stdout))
}

fn remote_sheprs_from_path_discovery(stdout: &str) -> Vec<RemoteShepr> {
    stdout.lines().filter_map(remote_shepr_from_path).collect()
}

fn remote_shepr_from_path_discovery(stdout: &str) -> Option<RemoteShepr> {
    stdout.lines().find_map(remote_shepr_from_path)
}

fn remote_shepr_from_path(path: &str) -> Option<RemoteShepr> {
    let path = path.trim();
    if !path.starts_with('/') {
        return None;
    }
    if is_mise_shim_path(path) {
        return None;
    }
    Some(RemoteShepr::new(path))
}

fn is_mise_shim_path(path: &str) -> bool {
    path.ends_with("/mise/shims/shepr")
}

fn remote_client_status(
    ssh: &RemoteSsh,
    remote_shepr: &RemoteShepr,
) -> io::Result<Option<RemoteClientStatusJson>> {
    let output = ssh.sh_output(&remote_shepr.status_client_command())?;
    if !output.status.success() {
        if output.status.code() == Some(255) {
            return Err(command_failed("remote SSH connection failed", &output));
        }
        return Ok(None);
    }
    Ok(parse_client_status_json(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteServerStatus {
    Running {
        version: Option<String>,
        protocol: Option<u32>,
        surface_interest: bool,
        health_check: bool,
        detached_server_daemon: bool,
    },
    NotRunning,
}

fn ensure_remote_server_ready(
    ssh: &RemoteSsh,
    remote_shepr: &RemoteShepr,
    require_surface_interest: bool,
) -> io::Result<()> {
    let status = remote_server_status(ssh, remote_shepr, require_surface_interest)?;
    let RemoteServerStatus::Running {
        version,
        protocol,
        surface_interest,
        health_check,
        detached_server_daemon,
    } = status
    else {
        return Ok(());
    };

    let Some(reason) = remote_server_restart_reason(
        protocol,
        detached_server_daemon,
        require_surface_interest,
        surface_interest,
        health_check,
    ) else {
        return Ok(());
    };

    if confirm_remote_server_stop(&ssh.destination(), version.as_deref(), reason)? {
        stop_remote_server(ssh, remote_shepr)?;
    }
    Ok(())
}

fn remote_server_status(
    ssh: &RemoteSsh,
    remote_shepr: &RemoteShepr,
    _require_surface_interest: bool,
) -> io::Result<RemoteServerStatus> {
    let command = remote_shepr.session_command(&ssh.session_name, &["status", "server", "--json"]);
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server status failed", &output));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_remote_server_status_json(stdout.trim())
}

#[derive(Debug, Deserialize)]
struct RemoteClientStatusJson {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    protocol: Option<u32>,
}

impl RemoteClientStatusJson {
    fn supports_endpoint_requirement(&self, _require_surface_interest: bool) -> bool {
        self.protocol == Some(crate::protocol::PROTOCOL_VERSION)
    }
}

#[derive(Debug, Deserialize)]
struct RemoteServerStatusJson {
    running: bool,
    version: Option<String>,
    protocol: Option<u32>,
    capabilities: Option<RemoteServerCapabilitiesJson>,
}

#[derive(Debug, Deserialize)]
struct RemoteServerCapabilitiesJson {
    detached_server_daemon: bool,
    surface_interest: bool,
    health_check: bool,
}

fn parse_client_status_json(status: &str) -> Option<RemoteClientStatusJson> {
    status
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<RemoteClientStatusJson>(line).ok())
        .find(|status| status.version.is_some() || status.protocol.is_some())
}

fn parse_remote_server_status_json(status: &str) -> io::Result<RemoteServerStatus> {
    let parsed: RemoteServerStatusJson = serde_json::from_str(status).map_err(|err| {
        io::Error::other(format!(
            "could not parse remote server status JSON from `{status}`: {err}"
        ))
    })?;
    if !parsed.running {
        return Ok(RemoteServerStatus::NotRunning);
    }

    let capabilities = parsed.capabilities;

    Ok(RemoteServerStatus::Running {
        version: parsed.version,
        protocol: parsed.protocol,
        surface_interest: capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.surface_interest),
        health_check: capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.health_check),
        detached_server_daemon: capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.detached_server_daemon),
    })
}

fn confirm_remote_server_stop(
    target: &str,
    version: Option<&str>,
    reason: RemoteServerRestartReason,
) -> io::Result<bool> {
    let required_restart = matches!(
        reason,
        RemoteServerRestartReason::EndpointProtocol
            | RemoteServerRestartReason::SurfaceInterest
            | RemoteServerRestartReason::HealthCheck
    );
    if !io::stdin().is_terminal() {
        if required_restart {
            return Err(io::Error::other(format!(
                "remote shepr server on {target} needs one final restart before this client can attach; run from an interactive terminal to approve restarting it"
            )));
        }

        eprintln!(
            "remote shepr server on {target} is still running v{}.",
            version_label(version)
        );
        return Ok(false);
    }

    eprintln!("remote shepr server on {target} is currently running:");
    eprintln!("  server: v{}", version_label(version));
    eprintln!();

    match reason {
        RemoteServerRestartReason::EndpointProtocol => {
            eprintln!(
                "the remote server predates Shepr's stable endpoint protocol and must restart before this client can attach."
            );
        }
        RemoteServerRestartReason::SurfaceInterest => {
            eprintln!(
                "the remote server must restart before it can join saved SSH endpoint federation."
            );
        }
        RemoteServerRestartReason::HealthCheck => {
            eprintln!("the remote server must restart to enable saved SSH endpoint health checks.");
        }
        RemoteServerRestartReason::DaemonDetach => {
            eprintln!(
                "the remote server was started by a shepr build that may not survive SSH connection loss. restart it so network drops disconnect only this client."
            );
        }
    }

    eprintln!(
        "This stops active remote pane processes, including shells, agents, dev servers, and tests."
    );
    let prompt = if required_restart {
        "stop the remote server and continue attaching? [y/N] "
    } else {
        "restart the remote server now? [y/N] "
    };
    eprint!("{prompt}");
    io::stderr().flush()?;

    if read_remote_confirmation(&mut io::stdin().lock(), false)? {
        return Ok(true);
    }
    if required_restart {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote shepr server stop cancelled",
        ));
    }

    Ok(false)
}

fn stop_remote_server(ssh: &RemoteSsh, remote_shepr: &RemoteShepr) -> io::Result<()> {
    let command = remote_shepr.session_command(&ssh.session_name, &["server", "stop"]);
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server stop failed", &output));
    }

    wait_for_remote_server_shutdown(ssh, remote_shepr)?;
    eprintln!(
        "stopped the remote shepr server on {}; it will restart when the remote client bridge attaches.",
        ssh.target()
    );
    Ok(())
}

fn wait_for_remote_server_shutdown(ssh: &RemoteSsh, remote_shepr: &RemoteShepr) -> io::Result<()> {
    let deadline = Instant::now() + REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT;
    loop {
        if remote_server_status(ssh, remote_shepr, false)? == RemoteServerStatus::NotRunning {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "shutdown was requested, but the old remote shepr server on {target} is still responding after {} seconds",
                    REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT.as_secs(),
                    target = ssh.target()
                ),
            ));
        }
        thread::sleep(REMOTE_SERVER_SHUTDOWN_POLL_INTERVAL);
    }
}

fn version_label(version: Option<&str>) -> &str {
    version.unwrap_or("unknown")
}

fn read_remote_confirmation(reader: &mut impl io::BufRead, default: bool) -> io::Result<bool> {
    let mut answer = String::new();
    if reader.read_line(&mut answer)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote setup cancelled",
        ));
    }
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        "" => Ok(default),
        _ => Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote setup cancelled: expected yes or no",
        )),
    }
}

fn posix_remote_api_discovery_command(session: &str) -> String {
    let script = format!(
        r#"set -f
candidates=$(
command -v shepr
{discovery}
)
IFS='
'
for candidate in $candidates; do
    case "$candidate" in
        */mise/shims/shepr) continue ;;
        /*) ;;
        *) continue ;;
    esac
    [ -x "$candidate" ] || continue
    if capability=$("$candidate" --session {session} remote-api-bridge --check </dev/null 2>/dev/null) && [ "$capability" = shepr-api-bridge-v1 ]; then
        printf '%s\n' "$candidate"
        exit 0
    fi
done
printf '%s\n' 'remote Shepr does not support machine API forwarding; update Shepr on this machine' >&2
exit 2"#,
        discovery = known_remote_binary_candidate_script(),
        session = shell_quote(session),
    );
    posix_shell_command(&posix_remote_output_command(&script))
}

pub(super) const STALE_API_METADATA: &str = "shepr-machine-metadata-stale-v1";

pub(super) fn cached_remote_api_command(
    metadata: &crate::client::endpoint::SshMachineMetadata,
    session: &str,
) -> String {
    let path = shell_quote(&metadata.executable);
    let session = shell_quote(session);
    let script = format!(
        "if capability=$({path} --session {session} remote-api-bridge --check </dev/null 2>/dev/null) && [ \"$capability\" = shepr-api-bridge-v1 ]; then\n{}\nelse\n    printf '%s\\n' '{STALE_API_METADATA}' >&2\n    exit 78\nfi",
        posix_remote_output_command(&format!(
            "exec {path} --session {session} remote-api-bridge"
        )),
    );
    posix_shell_command(&script)
}

fn reattach_command(
    program: &str,
    target: &str,
    session_name: &str,
    keybindings: RemoteKeybindings,
) -> String {
    let program = shell_quote(if program.is_empty() { "shepr" } else { program });
    let target = shell_quote(target);
    let mut command = format!("{program} --remote {target}");
    if keybindings != RemoteKeybindings::Local {
        command.push_str(" --remote-keybindings ");
        command.push_str(keybindings.as_str());
    }
    if session_name != crate::session::DEFAULT_SESSION_NAME {
        command.push_str(" --session ");
        command.push_str(&shell_quote(session_name));
    }
    command
}

fn command_failed(context: &str, output: &Output) -> io::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        io::Error::other(format!("{context}: {}", output.status))
    } else {
        io::Error::other(format!("{context}: {stderr}"))
    }
}

pub(super) struct SshStdioBridge {
    local_socket: PathBuf,
    socket_identity: crate::ipc::SocketFileIdentity,
    should_stop: Arc<AtomicBool>,
    failure_rx: mpsc::Receiver<io::Error>,
    thread: Option<JoinHandle<()>>,
    // Dropped after `Drop::drop` has removed the socket; see `TeardownRegistry`.
    _teardown: TeardownRegistration,
}

impl SshStdioBridge {
    pub(super) fn start(
        target: String,
        remote_shepr: &RemoteShepr,
        local_socket: PathBuf,
        session_name: &str,
        ssh_options: Option<&ManagedSshOptions>,
        noninteractive: bool,
    ) -> io::Result<Self> {
        Self::start_command(
            target,
            remote_shepr.bridge_command(session_name, noninteractive),
            local_socket,
            ssh_options,
            noninteractive,
        )
    }

    pub(super) fn start_command(
        target: String,
        remote_command: String,
        local_socket: PathBuf,
        ssh_options: Option<&ManagedSshOptions>,
        noninteractive: bool,
    ) -> io::Result<Self> {
        crate::ipc::prepare_socket_path(&local_socket, |path| {
            format!("remote bridge is already listening at {}", path.display())
        })?;
        let listener = crate::ipc::bind_private_local_listener(&local_socket)?;
        let socket_identity = crate::ipc::socket_file_identity(&local_socket)?;
        let teardown = SSH_TEARDOWN.register(TeardownResource::Socket {
            path: local_socket.clone(),
            identity: socket_identity.clone(),
        });
        if let Err(err) =
            crate::ipc::restrict_socket_permissions(&local_socket, BRIDGE_SOCKET_PERMISSION_MODE)
        {
            let _ = crate::ipc::remove_socket_file_if_owned(&local_socket, &socket_identity);
            return Err(err);
        }
        if let Err(err) = listener.set_nonblocking(ListenerNonblockingMode::Accept) {
            let _ = crate::ipc::remove_socket_file_if_owned(&local_socket, &socket_identity);
            return Err(err);
        }

        let should_stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&should_stop);
        let thread_ssh_options = ssh_options.cloned();
        let (failure_tx, failure_rx) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok(stream) => {
                        let stream = match prepare_remote_bridge_stream(stream) {
                            Ok(stream) => stream,
                            Err(err) => {
                                tracing::error!(
                                    error = %err,
                                    "remote bridge failed to prepare client socket"
                                );
                                continue;
                            }
                        };
                        if let Err(err) = bridge_connection(
                            stream,
                            &target,
                            &remote_command,
                            thread_ssh_options.as_ref(),
                            noninteractive,
                            &thread_stop,
                        ) {
                            if noninteractive {
                                tracing::warn!(error = %err, "saved SSH endpoint bridge failed");
                            } else {
                                eprintln!("shepr: remote bridge failed: {err}");
                            }
                            // The original error, so an `SshBridgeExit` payload survives.
                            let _ = failure_tx.try_send(err);
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(BRIDGE_ACCEPT_POLL);
                    }
                    Err(err) => {
                        if noninteractive {
                            tracing::warn!(error = %err, "saved SSH endpoint listener failed");
                        } else {
                            eprintln!("shepr: remote bridge listener failed: {err}");
                        }
                        break;
                    }
                }
            }
        });

        Ok(Self {
            local_socket,
            socket_identity,
            should_stop,
            failure_rx,
            thread: Some(thread),
            _teardown: teardown,
        })
    }

    pub(super) fn reported_failure(&self) -> Option<io::Error> {
        self.failure_rx
            .recv_timeout(BRIDGE_FAILURE_REPORT_TIMEOUT)
            .ok()
    }
}

fn prepare_remote_bridge_stream(
    mut stream: crate::ipc::LocalStream,
) -> io::Result<crate::ipc::LocalStream> {
    crate::ipc::set_local_stream_polling(&mut stream, false)?;
    Ok(stream)
}

impl Drop for SshStdioBridge {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Release);
        let _ = crate::ipc::remove_socket_file_if_owned(&self.local_socket, &self.socket_identity);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn ssh_config_quote(path: &str) -> String {
    format!("\"{path}\"")
}

/// Returns the quoted `Include` value for an SSH config file, or `None` when
/// there is no such file.
fn ssh_config_include(path: Option<&Path>) -> Option<String> {
    path.filter(|path| path.is_file())
        .map(|path| ssh_config_quote(&path.to_string_lossy()))
}

/// Builds a temporary ssh config that includes the user's settings first, so
/// OpenSSH's first-value-wins behavior preserves explicit user keepalives.
fn write_managed_ssh_config(target: &str) -> io::Result<ManagedSshConfig> {
    let paths = crate::platform::remote_ssh_config_paths();
    let control_path = Some(crate::platform::shared_ssh_control_path(
        &crate::config::config_path(),
        target,
    )?);

    let dir = crate::platform::create_remote_ssh_config_dir(SSH_CONTROL_SOCKET_NAME)?;
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
        let mut file = crate::platform::create_remote_ssh_config_file(&path)?;
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

struct BridgeUploadStop {
    stopped: AtomicBool,
    wake: crate::platform::RemoteBridgeWake,
}

impl BridgeUploadStop {
    fn new() -> io::Result<Self> {
        Ok(Self {
            stopped: AtomicBool::new(false),
            wake: crate::platform::RemoteBridgeWake::new()?,
        })
    }

    fn cancel(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel)
            && let Err(error) = self.wake.cancel()
        {
            tracing::debug!(%error, "remote bridge read cancellation failed");
        }
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

#[cfg(test)]
pub(crate) fn bridge_upload_cancellation_for_test(
    stream: crate::ipc::LocalStream,
    mut writer: impl io::Write + Send + 'static,
) -> impl FnOnce() {
    stream
        .set_nonblocking(true)
        .expect("test stream supports nonblocking mode");
    let stop =
        Arc::new(BridgeUploadStop::new().expect("test bridge upload stop creation succeeds"));
    let worker_stop = Arc::clone(&stop);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let closed = AtomicBool::new(false);
        let result = copy_local_stream_to_writer(
            stream,
            &mut writer,
            &worker_stop,
            &AtomicBool::new(false),
            &closed,
        );
        done_tx
            .send((result, closed.load(Ordering::Acquire)))
            .expect("test done channel is open");
    });
    move || {
        stop.cancel();
        let (result, closed) = done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("test worker reports completion within timeout");
        worker.join().expect("test worker thread does not panic");
        result.expect("upload copy completes without error");
        assert!(!closed, "upload cancellation must not report peer EOF");
    }
}

fn bridge_connection(
    mut stream: crate::ipc::LocalStream,
    target: &str,
    remote_command: &str,
    ssh_options: Option<&ManagedSshOptions>,
    noninteractive: bool,
    bridge_stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    let upload_stop = Arc::new(BridgeUploadStop::new()?);
    let mut command = Command::new("ssh");
    apply_managed_ssh_options(&mut command, ssh_options);
    if noninteractive {
        apply_noninteractive_ssh_options(&mut command);
    }
    command
        .arg("-T")
        .arg(target)
        .arg(remote_command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if noninteractive {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });

    let mut child = command
        .spawn()
        .map_err(|err| io::Error::new(err.kind(), format!("failed to start ssh bridge: {err}")))?;
    let mut child_stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => return terminate_bridge_child(child, "ssh bridge stdin missing"),
    };
    let child_stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => return terminate_bridge_child(child, "ssh bridge stdout missing"),
    };
    let stderr_reader = if noninteractive {
        let Some(child_stderr) = child.stderr.take() else {
            return terminate_bridge_child(child, "ssh bridge stderr missing");
        };
        Some(PipeCapture::spawn(
            child_stderr,
            NONINTERACTIVE_SSH_STDERR_LIMIT,
            PipeEcho::None,
        ))
    } else {
        None
    };
    let stream_to_child = match stream.try_clone() {
        Ok(stream) => stream,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
    };
    if let Err(err) = crate::ipc::set_local_stream_polling(&mut stream, true) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }
    let mut child_to_stream = stream;

    let connection_stop = Arc::new(AtomicBool::new(false));
    let upload_failed = Arc::new(AtomicBool::new(false));
    let download_done = Arc::new(AtomicBool::new(false));
    let client_closed = Arc::new(AtomicBool::new(false));
    let upload_cancel = Arc::clone(&upload_stop);
    let upload_bridge_stop = Arc::clone(bridge_stop);
    let upload_failed_worker = Arc::clone(&upload_failed);
    let upload_client_closed = Arc::clone(&client_closed);
    let upload = thread::spawn(move || {
        let result = copy_local_stream_to_writer(
            stream_to_child,
            &mut child_stdin,
            &upload_cancel,
            &upload_bridge_stop,
            &upload_client_closed,
        );
        upload_failed_worker.store(result.is_err(), Ordering::Release);
        result
    });
    let download_stop = Arc::clone(&connection_stop);
    let download_bridge_stop = Arc::clone(bridge_stop);
    let download_done_worker = Arc::clone(&download_done);
    let download_upload_stop = Arc::clone(&upload_stop);
    let download = thread::spawn(move || {
        let mut child_stdout = io::BufReader::new(child_stdout);
        let result = discard_remote_output_preamble(&mut child_stdout).and_then(|()| {
            copy_reader_to_local_stream(
                &mut child_stdout,
                &mut child_to_stream,
                &download_stop,
                &download_bridge_stop,
            )
        });
        download_done_worker.store(true, Ordering::Release);
        download_upload_stop.cancel();
        result
    });

    let mut stopped_at = None;
    let (status_result, child_exited) = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                upload_stop.cancel();
                break (Ok(status), true);
            }
            Ok(None) => {}
            Err(err) => {
                connection_stop.store(true, Ordering::Release);
                upload_stop.cancel();
                let _ = child.kill();
                let _ = child.wait();
                break (Err(err), false);
            }
        }
        if bridge_stop.load(Ordering::Acquire) {
            connection_stop.store(true, Ordering::Release);
            upload_stop.cancel();
            let _ = child.kill();
            break (child.wait(), false);
        }
        if client_closed.load(Ordering::Acquire)
            || upload_failed.load(Ordering::Acquire)
            || download_done.load(Ordering::Acquire)
        {
            upload_stop.cancel();
            let stopped_at = stopped_at.get_or_insert_with(Instant::now);
            if stopped_at.elapsed() >= Duration::from_millis(250) {
                connection_stop.store(true, Ordering::Release);
                let _ = child.kill();
                break (child.wait(), false);
            }
        }
        thread::sleep(BRIDGE_ACCEPT_POLL);
    };
    upload_stop.cancel();
    if !child_exited {
        connection_stop.store(true, Ordering::Release);
    }
    let upload_result = upload
        .join()
        .map_err(|_| io::Error::other("remote bridge upload worker panicked"))?;
    let download_result = download
        .join()
        .map_err(|_| io::Error::other("remote bridge download worker panicked"))?;
    // Bounded: a ControlPersist master forked by this ssh can hold its stderr open for
    // the whole persist timeout after the bridge itself has exited.
    let stderr = match stderr_reader {
        Some(reader) => reader.finish(PIPE_DRAIN_GRACE)?,
        None => Vec::new(),
    };
    let status = status_result?;

    let stopping = bridge_stop.load(Ordering::Acquire);
    let client_closed = client_closed.load(Ordering::Acquire);
    if child_exited && !status.success() && !stopping && !client_closed {
        return Err(ssh_bridge_exit_error(status, &stderr));
    }
    if !stopping && !client_closed {
        upload_result.map_err(|err| {
            io::Error::new(err.kind(), format!("remote bridge upload failed: {err}"))
        })?;
        download_result.map_err(|err| {
            io::Error::new(err.kind(), format!("remote bridge download failed: {err}"))
        })?;
    }

    if status.success() || stopping || client_closed {
        Ok(())
    } else {
        Err(ssh_bridge_exit_error(status, &stderr))
    }
}

fn ssh_bridge_exit_error(status: std::process::ExitStatus, stderr: &[u8]) -> io::Error {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    let message = if stderr.is_empty() {
        format!("ssh bridge exited with {status}")
    } else {
        format!("remote SSH connection failed: {stderr}")
    };
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        SshBridgeExit {
            code: status.code(),
            message,
        },
    )
}

/// An ssh bridge process that exited unsuccessfully, keeping its exit code so
/// callers can tell ssh's own failures from the remote command's.
#[derive(Debug)]
struct SshBridgeExit {
    code: Option<i32>,
    message: String,
}

impl std::fmt::Display for SshBridgeExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SshBridgeExit {}

/// OpenSSH exits with 255 when ssh itself fails (resolve, connect, host key,
/// authentication, a dropped link); any other code came from the remote command.
const SSH_OWN_FAILURE_EXIT_CODE: i32 = 255;

/// Whether `error` says the SSH link, not the remote side, failed: the remote end
/// was never reached or was lost, so nothing is known about the remote install.
pub(super) fn is_ssh_link_failure(error: &io::Error) -> bool {
    if let Some(exit) = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<SshBridgeExit>())
    {
        return exit.code == Some(SSH_OWN_FAILURE_EXIT_CODE);
    }
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::AddrInUse
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::NetworkDown
    )
}

fn discard_remote_output_preamble(reader: &mut impl io::BufRead) -> io::Result<()> {
    let marker = REMOTE_OUTPUT_READY_MARKER.as_bytes();
    let mut matched = 0;
    let mut matching = true;
    loop {
        let (consumed, ready) = {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                if matching && matched == marker.len() {
                    return Ok(());
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "remote command exited before producing its output marker",
                ));
            }

            let mut consumed = 0;
            let mut ready = false;
            for &byte in buffer {
                consumed += 1;
                if byte == b'\n' {
                    if matching && matched == marker.len() {
                        ready = true;
                        break;
                    }
                    matched = 0;
                    matching = true;
                } else if matching && matched < marker.len() && byte == marker[matched] {
                    matched += 1;
                } else if matching && (matched != marker.len() || byte != b'\r') {
                    matching = false;
                }
            }
            (consumed, ready)
        };
        reader.consume(consumed);
        if ready {
            return Ok(());
        }
    }
}

fn terminate_bridge_child(mut child: std::process::Child, message: &'static str) -> io::Result<()> {
    let _ = child.kill();
    let _ = child.wait();
    Err(io::Error::new(io::ErrorKind::BrokenPipe, message))
}

fn copy_reader_to_local_stream<R: io::Read>(
    reader: &mut R,
    stream: &mut crate::ipc::LocalStream,
    connection_stop: &AtomicBool,
    bridge_stop: &AtomicBool,
) -> io::Result<u64> {
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0;

    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(total),
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        let mut written = 0;
        while written < read {
            if connection_stop.load(Ordering::Acquire) || bridge_stop.load(Ordering::Acquire) {
                return Ok(total);
            }
            let chunk_len = (read - written).min(4 * 1024);
            match stream.write(&buffer[written..written + chunk_len]) {
                Ok(0) => thread::sleep(BRIDGE_IO_POLL),
                Ok(count) => written += count,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(BRIDGE_IO_POLL);
                }
                Err(err) => return Err(err),
            }
        }
        stream.flush()?;
        total += read as u64;
    }
}

fn copy_local_stream_to_writer<W: io::Write>(
    mut stream: crate::ipc::LocalStream,
    writer: &mut W,
    connection_stop: &BridgeUploadStop,
    bridge_stop: &AtomicBool,
    client_closed: &AtomicBool,
) -> io::Result<u64> {
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0;

    while !connection_stop.is_stopped() && !bridge_stop.load(Ordering::Acquire) {
        #[cfg(test)]
        tests::UPLOAD_READ_ATTEMPTS.with(|attempts| {
            if let Some(attempts) = attempts.borrow().as_ref() {
                attempts.fetch_add(1, Ordering::Relaxed);
            }
        });
        match crate::ipc::poll_local_stream_read_count(&mut stream, &mut buffer)? {
            crate::ipc::LocalStreamReadCount::Data(read) => {
                writer.write_all(&buffer[..read])?;
                writer.flush()?;
                total += read as u64;
            }
            crate::ipc::LocalStreamReadCount::Pending => {
                connection_stop.wake.wait(&stream)?;
            }
            crate::ipc::LocalStreamReadCount::Closed => {
                client_closed.store(true, Ordering::Release);
                break;
            }
        }
    }

    Ok(total)
}

fn run_client_process(
    local_socket: &Path,
    reattach_command: &str,
    keybindings: RemoteKeybindings,
) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let status = Command::new(exe)
        .arg("client")
        .env(
            crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
            local_socket,
        )
        .env(REATTACH_COMMAND_ENV_VAR, reattach_command)
        .env(REMOTE_KEYBINDINGS_ENV_VAR, keybindings.as_str())
        .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            format!("remote client exited with {status}"),
        ))
    }
}

fn local_forward_socket_path(target: &str, session_name: &str) -> PathBuf {
    let pid = std::process::id();
    let target_clean = sanitize_path_component(target);
    let session_clean = sanitize_path_component(session_name);
    let readable_name = format!("shepr-remote-{pid}-{target_clean}-{session_clean}.sock");
    let target_prefix: String = target_clean.chars().take(8).collect();
    let hash = short_socket_hash(target, session_name);
    let short_name = format!("shepr-r-{pid}-{target_prefix}-{hash}.sock");
    crate::platform::remote_bridge_endpoint_path(&readable_name, &short_name)
}

#[cfg(test)]
fn fits_unix_socket_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().len() <= 103
}

fn short_socket_hash(target: &str, session: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    target.hash(&mut hasher);
    0u8.hash(&mut hasher);
    session.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn sanitize_path_component(input: &str) -> String {
    let sanitized: String = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect();

    sanitized.trim_matches('-').chars().take(32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        pub(super) static UPLOAD_READ_ATTEMPTS: std::cell::RefCell<Option<Arc<std::sync::atomic::AtomicUsize>>> = const { std::cell::RefCell::new(None) };
    }

    fn upload_test_streams(name: &str) -> (crate::ipc::LocalStream, crate::ipc::LocalStream) {
        let socket = local_forward_socket_path(name, "upload-test");
        let listener = crate::ipc::bind_private_local_listener(&socket).expect("test precondition");
        let client = crate::ipc::connect_local_stream(&socket).expect("test precondition");
        let server = listener.accept().expect("test precondition");
        server.set_nonblocking(true).expect("test precondition");
        drop(listener);
        std::fs::remove_file(socket).expect("test precondition");
        (client, server)
    }

    #[test]
    fn bridge_upload_idle_waits_without_repeated_reads_and_cancels() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::mpsc;

        let (mut client, stream) = upload_test_streams("idle");
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = Arc::clone(&attempts);
        let stop = Arc::new(BridgeUploadStop::new().expect("test precondition"));
        let worker_stop = Arc::clone(&stop);
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            UPLOAD_READ_ATTEMPTS.with(|slot| *slot.borrow_mut() = Some(worker_attempts));
            let mut output = Vec::new();
            let closed = AtomicBool::new(false);
            let result = copy_local_stream_to_writer(
                stream,
                &mut output,
                &worker_stop,
                &AtomicBool::new(false),
                &closed,
            );
            done_tx
                .send((result, output, closed.load(Ordering::Acquire)))
                .expect("test precondition");
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while attempts.load(Ordering::Relaxed) == 0 {
            assert!(Instant::now() < deadline, "upload worker did not start");
            thread::sleep(Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(100));
        let idle_reads = attempts.load(Ordering::Relaxed);
        client.write_all(b"pane input").expect("test precondition");
        let deadline = Instant::now() + Duration::from_secs(5);
        while attempts.load(Ordering::Relaxed) < idle_reads + 2 {
            assert!(
                Instant::now() < deadline,
                "input did not wake the upload worker"
            );
            thread::sleep(Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(100));
        let reads_after_input = attempts.load(Ordering::Relaxed);
        stop.cancel();
        let (result, output, closed) = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test precondition");
        worker.join().expect("test precondition");
        assert_eq!(result.expect("test precondition"), 10);
        assert_eq!(output, b"pane input");
        assert!(!closed, "cancellation is not a peer disconnect");
        assert_eq!(idle_reads, 1, "idle forwarding must wait, not retry reads");
        assert_eq!(
            reads_after_input, 3,
            "forwarding must sleep again after input"
        );
    }

    #[test]
    fn bridge_upload_cancel_before_wait_preserves_download() {
        use std::io::Read as _;

        let (mut client, stream) = upload_test_streams("cancel-before-wait");
        let mut download = stream.try_clone().expect("test precondition");
        let stop = BridgeUploadStop::new().expect("test precondition");
        stop.cancel();
        stop.cancel();
        let closed = AtomicBool::new(false);
        let count = copy_local_stream_to_writer(
            stream,
            &mut Vec::new(),
            &stop,
            &AtomicBool::new(false),
            &closed,
        )
        .expect("test precondition");
        assert_eq!(count, 0);
        assert!(!closed.load(Ordering::Acquire));
        download
            .write_all(b"final frame")
            .expect("test precondition");
        let mut output = [0; 11];
        client.read_exact(&mut output).expect("test precondition");
        assert_eq!(&output, b"final frame");
    }

    #[test]
    fn bridge_upload_cancel_between_stop_check_and_wait_is_retained() {
        let (_client, stream) = upload_test_streams("cancel-before-poll");
        let stop = BridgeUploadStop::new().expect("test precondition");
        assert!(!stop.is_stopped());
        stop.cancel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            done_tx
                .send(stop.wake.wait(&stream))
                .expect("test precondition");
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("test precondition")
            .expect("test precondition");
        worker.join().expect("test precondition");
    }

    #[test]
    fn bridge_upload_drains_input_before_peer_eof() {
        let (mut client, stream) = upload_test_streams("drain");
        let payload = vec![b'x'; 1024 * 1024];
        let expected = payload.clone();
        let worker = thread::spawn(move || {
            let stop = BridgeUploadStop::new().expect("test precondition");
            let mut output = Vec::new();
            let closed = AtomicBool::new(false);
            let count = copy_local_stream_to_writer(
                stream,
                &mut output,
                &stop,
                &AtomicBool::new(false),
                &closed,
            )
            .expect("test precondition");
            assert!(closed.load(Ordering::Acquire));
            assert_eq!(count, output.len() as u64);
            output
        });
        client.write_all(&payload).expect("test precondition");
        drop(client);
        assert_eq!(worker.join().expect("test precondition"), expected);
    }

    #[test]
    fn bridge_socket_is_user_only() {
        use std::os::unix::fs::PermissionsExt;

        let socket = std::env::temp_dir().join(format!(
            "shepr-bridge-permissions-test-{}.sock",
            std::process::id()
        ));
        let remote_shepr = RemoteShepr::new("/usr/bin/shepr");
        let bridge = SshStdioBridge::start(
            "example".to_string(),
            &remote_shepr,
            socket.clone(),
            "default",
            None,
            false,
        )
        .expect("start bridge listener");

        let mode = std::fs::metadata(&socket)
            .expect("test precondition")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, BRIDGE_SOCKET_PERMISSION_MODE);

        drop(bridge);
        let _ = std::fs::remove_file(socket);
    }

    #[test]
    fn accepted_bridge_stream_is_reset_to_blocking() {
        use std::os::fd::AsRawFd as _;

        fn is_nonblocking(stream: &crate::ipc::LocalStream) -> bool {
            let fd = match stream {
                crate::ipc::LocalStream::UdSocket(stream) => stream.inner().as_raw_fd(),
            };
            // SAFETY: F_GETFL only reads flags from the live descriptor owned by `stream`.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert!(flags >= 0, "fcntl(F_GETFL): {}", io::Error::last_os_error());
            flags & libc::O_NONBLOCK != 0
        }

        let socket = std::env::temp_dir().join(format!(
            "shepr-bridge-blocking-test-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket);
        let listener = crate::ipc::bind_private_local_listener(&socket).expect("bind listener");
        let client = crate::ipc::connect_local_stream(&socket).expect("connect client");
        let mut server = listener.accept().expect("accept client");

        crate::ipc::set_local_stream_polling(&mut server, true)
            .expect("force a nonblocking accepted stream");
        assert!(is_nonblocking(&server));
        let server = prepare_remote_bridge_stream(server).expect("prepare bridge stream");
        assert!(!is_nonblocking(&server));

        drop(server);
        drop(client);
        drop(listener);
        let _ = std::fs::remove_file(socket);
    }

    #[test]
    fn bridge_drop_while_waiting_for_client_is_bounded() {
        let socket = local_forward_socket_path("drop-test", "default");
        let remote_shepr = RemoteShepr::new("/usr/bin/shepr");
        let bridge = SshStdioBridge::start(
            "example".to_string(),
            &remote_shepr,
            socket.clone(),
            "default",
            None,
            false,
        )
        .expect("start bridge listener");
        let started = Instant::now();

        drop(bridge);

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!socket.exists());
    }

    #[test]
    fn managed_ssh_config_includes_user_config_then_fallback() {
        use std::os::unix::fs::PermissionsExt;

        let managed_config = write_managed_ssh_config("example").expect("write managed config");
        let path = managed_config.options.config_path.clone();
        let control_path = managed_config
            .options
            .control_path
            .clone()
            .expect("Unix managed config has a control path");
        let contents = std::fs::read_to_string(&path).expect("read keepalive config");

        // shepr's fallback transport settings are present...
        assert!(
            contents.contains("Host *"),
            "config should add a Host * fallback block: {contents}"
        );
        assert!(
            contents.contains("ServerAliveInterval 15"),
            "config should set the keepalive interval: {contents}"
        );
        assert!(
            contents.contains("ServerAliveCountMax 4"),
            "config should set the keepalive count: {contents}"
        );
        assert!(!contents.contains("ControlMaster"));
        assert!(!contents.contains("ControlPersist"));
        assert!(!contents.contains("ControlPath"));
        // ...and any user config is Included (quoted) BEFORE it so
        // first-value-wins keeps the user's own settings.
        if let Some(home) = std::env::var_os("HOME") {
            let user_config = PathBuf::from(home).join(".ssh").join("config");
            if user_config.is_file() {
                let include = format!(
                    "Include {}",
                    ssh_config_quote(&user_config.to_string_lossy())
                );
                let include_at = contents.find(&include).expect("user config Included");
                let fallback_at = contents.find("Host *").expect("fallback present");
                assert!(
                    include_at < fallback_at,
                    "user config must be Included before shepr's fallback: {contents}"
                );
            }
        }

        let mode = std::fs::metadata(&path)
            .expect("test precondition")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, BRIDGE_SOCKET_PERMISSION_MODE,
            "keepalive config must be user-only"
        );
        // The config lives in a private 0700 dir, not a predictable temp path.
        let dir = path.parent().expect("config has a parent dir");
        let dir_mode = std::fs::metadata(dir)
            .expect("test precondition")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "ssh config dir must be user-only");
        assert!(
            fits_unix_socket_path(&control_path),
            "control socket path must fit portable Unix socket limits"
        );

        drop(managed_config);
    }

    #[test]
    fn shared_ssh_transport_survives_helper_config_drop() {
        let first = write_managed_ssh_config("example").expect("test precondition");
        let second = write_managed_ssh_config("example").expect("test precondition");
        let socket = first
            .options
            .control_path
            .clone()
            .expect("test precondition");
        assert_eq!(Some(&socket), second.options.control_path.as_ref());
        assert_ne!(socket.parent(), first.options.config_path.parent());
        let config_path = first.options.config_path.clone();
        drop(first);
        assert!(!config_path.exists());
        assert!(socket.parent().expect("test precondition").is_dir());
    }

    #[test]
    fn ssh_authentication_diagnostics_are_narrow() {
        for message in [
            "user@host: Permission denied (publickey).",
            "Permission denied (keyboard-interactive,password).",
            "Permission denied (password).",
            "sign_and_send_pubkey: signing failed for ED25519 from agent: agent refused operation",
        ] {
            assert!(ssh_error_requires_authentication(message), "{message}");
        }
        for message in [
            "Host key verification failed.",
            "REMOTE HOST IDENTIFICATION HAS CHANGED!",
            "Permission denied opening /tmp/file",
            "Connection refused",
            "agent disconnected",
            "Permission denied (publickey). Host key verification failed.",
        ] {
            assert!(!ssh_error_requires_authentication(message), "{message}");
        }
    }

    #[test]
    fn bridge_options_keep_temporary_config_alive_after_helper_drop() {
        let config = write_managed_ssh_config("example").expect("test precondition");
        let path = config.options.config_path.clone();
        let worker_options = config.options.clone();
        drop(config);
        assert!(path.is_file());
        drop(worker_options);
        assert!(!path.exists());
    }

    #[test]
    fn authentication_command_uses_shared_transport_without_askpass_or_host_key_relaxation() {
        let config = write_managed_ssh_config("example").expect("test precondition");
        let setup = RemoteSsh::new("example".into(), true, "other-session".into());
        assert_eq!(
            config.options.control_path,
            setup.options().expect("test precondition").control_path
        );
        let authentication = authentication_command_with_config("example", config);
        let command = &authentication.command;
        assert_eq!(command.get_program(), "ssh");
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();
        for required in [
            "ControlMaster=auto",
            "ControlPersist=600",
            "BatchMode=no",
            "StrictHostKeyChecking=yes",
        ] {
            assert!(args.iter().any(|arg| arg == required), "missing {required}");
        }
        assert_eq!(&args[args.len() - 3..], &["-T", "example", "exit"]);
        let env = command.get_envs().collect::<Vec<_>>();
        assert!(env.iter().any(
            |(key, value)| *key == std::ffi::OsStr::new("SSH_ASKPASS_REQUIRE")
                && *value == Some(std::ffi::OsStr::new("never"))
        ));
        assert!(
            env.iter()
                .any(|(key, value)| *key == std::ffi::OsStr::new("SSH_ASKPASS") && value.is_none())
        );
    }

    #[test]
    fn authentication_command_rejects_option_injection() {
        assert_eq!(
            ssh_authentication_command("-oProxyCommand=bad")
                .err()
                .expect("test precondition")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn unmanaged_ssh_setup_preserves_plain_transport() {
        let ssh = RemoteSsh::new("example".into(), false, "main".into());
        assert!(ssh.options().is_none());
        assert!(!ssh.command().get_args().any(|arg| arg == "-F"));
    }

    #[test]
    fn ssh_config_quote_wraps_path_with_spaces() {
        assert_eq!(
            ssh_config_quote("/home/a b/.ssh/config"),
            "\"/home/a b/.ssh/config\""
        );
    }

    #[test]
    fn remote_ssh_command_uses_managed_config_when_present() {
        let managed_config = write_managed_ssh_config("example").expect("write managed config");
        let config_path = managed_config.options.config_path.clone();
        let control_path = managed_config
            .options
            .control_path
            .clone()
            .expect("test precondition");
        let ssh = RemoteSsh {
            target: "example".to_string(),
            session_name: crate::session::DEFAULT_SESSION_NAME.into(),
            managed_config: Some(managed_config),
            noninteractive: false,
        };

        let command = ssh.command();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            vec![
                "-C".to_string(),
                "-F".to_string(),
                config_path.to_string_lossy().into_owned(),
                "-S".to_string(),
                control_path.to_string_lossy().into_owned(),
                "-o".to_string(),
                "ControlMaster=auto".to_string(),
                "-o".to_string(),
                "ControlPersist=600".to_string(),
                "-T".to_string(),
                "example".to_string(),
            ]
        );
    }

    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt as _;
        std::process::ExitStatus::from_raw(code << 8)
    }

    #[test]
    fn only_ssh_own_exit_code_counts_as_a_link_failure() {
        let link = ssh_bridge_exit_error(exit_status(255), b"Connection refused");
        assert!(is_ssh_link_failure(&link));
        assert_eq!(link.kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(
            link.to_string(),
            "remote SSH connection failed: Connection refused"
        );
        let missing =
            ssh_bridge_exit_error(exit_status(127), b"sh: 1: exec: /old/shepr: not found");
        assert!(!is_ssh_link_failure(&missing));
        assert!(is_ssh_link_failure(&io::Error::new(
            io::ErrorKind::TimedOut,
            "handshake timed out"
        )));
        assert!(!is_ssh_link_failure(&io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "closed before welcome"
        )));
    }

    #[test]
    fn exit_sweep_removes_what_owners_left_behind() {
        let registry: &'static TeardownRegistry = Box::leak(Box::new(TeardownRegistry::new()));
        let root = std::env::temp_dir().join(format!(
            "shepr-teardown-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        let leaked = root.join("leaked");
        let released = root.join("released");
        fs::create_dir_all(&leaked).expect("test precondition");
        fs::create_dir_all(&released).expect("test precondition");

        // An owner still alive at exit (a writer thread that has not run its drop yet).
        let _stuck = registry.register(TeardownResource::Directory(leaked.clone()));
        // An owner that finished its own teardown: it is no longer the sweep's business.
        drop(registry.register(TeardownResource::Directory(released.clone())));

        let started = Instant::now();
        registry.release_all(Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            !leaked.exists(),
            "the sweep removes what is still registered"
        );
        assert!(released.exists(), "deregistered resources are left alone");
        fs::remove_dir_all(root).expect("test precondition");
    }

    #[test]
    fn exit_sweep_waits_for_owners_that_are_already_dropping() {
        let registry: &'static TeardownRegistry = Box::leak(Box::new(TeardownRegistry::new()));
        let registration = registry.register(TeardownResource::Directory(PathBuf::from(
            "/nonexistent/shepr-teardown-test",
        )));
        let owner = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            drop(registration);
        });
        let started = Instant::now();
        registry.release_all(Duration::from_secs(5));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "returns as soon as the owner deregisters"
        );
        owner.join().expect("test precondition");
    }

    #[test]
    fn noninteractive_ssh_stderr_capture_is_bounded() {
        let stderr = vec![b'x'; NONINTERACTIVE_SSH_STDERR_LIMIT + 4096];
        let captured = PipeCapture::spawn(
            io::Cursor::new(stderr),
            NONINTERACTIVE_SSH_STDERR_LIMIT,
            PipeEcho::None,
        )
        .finish(Duration::from_secs(3))
        .expect("capture stderr");
        assert_eq!(captured.len(), NONINTERACTIVE_SSH_STDERR_LIMIT);
    }

    #[test]
    fn noninteractive_ssh_command_cannot_prompt_or_accept_unknown_hosts() {
        let ssh = RemoteSsh::new_noninteractive("example".into());
        let args = ssh
            .command()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for required in [
            "-C",
            "BatchMode=yes",
            "NumberOfPasswordPrompts=0",
            "StrictHostKeyChecking=yes",
            "ConnectTimeout=10",
            "ConnectionAttempts=1",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=4",
        ] {
            assert!(args.iter().any(|arg| arg == required), "missing {required}");
        }
        assert_eq!(args.iter().any(|arg| arg == "-F"), ssh.options().is_some());
    }

    #[test]
    fn remote_setup_approval_requires_input_and_rejects_unrecognized_answers() {
        for default in [false, true] {
            for input in ["", "maybe\n"] {
                assert_eq!(
                    read_remote_confirmation(&mut input.as_bytes(), default)
                        .expect_err("test precondition")
                        .kind(),
                    io::ErrorKind::Interrupted
                );
            }
            assert_eq!(
                read_remote_confirmation(&mut "\n".as_bytes(), default).expect("test precondition"),
                default
            );
            assert!(
                read_remote_confirmation(&mut "YES\n".as_bytes(), default)
                    .expect("test precondition")
            );
            assert!(
                !read_remote_confirmation(&mut "no\n".as_bytes(), default)
                    .expect("test precondition")
            );
        }
    }

    #[test]
    fn saved_machine_compatibility_requires_matching_protocol() {
        let mut status = RemoteClientStatusJson {
            version: Some(crate::build_info::version()),
            protocol: Some(crate::protocol::PROTOCOL_VERSION),
        };
        assert!(status.supports_endpoint_requirement(true));
        status.protocol = Some(crate::protocol::PROTOCOL_VERSION + 1);
        assert!(!status.supports_endpoint_requirement(true));
        status.protocol = None;
        assert!(!status.supports_endpoint_requirement(true));
    }

    #[test]
    fn saved_machine_server_commands_are_scoped_to_the_explicit_session() {
        let shepr = RemoteShepr::new("/usr/bin/shepr");
        for (args, command) in [
            (&["status", "server", "--json"][..], "status server --json"),
            (&["server", "stop"][..], "server stop"),
            (&["remote-client-bridge"][..], "remote-client-bridge"),
        ] {
            assert_eq!(
                shepr.session_command("agents", args),
                format!("{} --session agents {command}", shepr.path)
            );
            assert_eq!(
                shepr.session_command(crate::session::DEFAULT_SESSION_NAME, args),
                format!("{} {command}", shepr.path)
            );
        }
    }

    #[test]
    fn remote_ssh_commands_compress_without_managed_config() {
        let ssh = RemoteSsh {
            target: "example".to_string(),
            session_name: crate::session::DEFAULT_SESSION_NAME.into(),
            managed_config: None,
            noninteractive: false,
        };

        let command = ssh.command();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(args, vec!["-C", "-T", "example"]);
    }

    #[test]
    fn sanitize_path_component_removes_shell_sensitive_chars() {
        assert_eq!(sanitize_path_component("user@host:22"), "user-host-22");
    }

    #[test]
    fn machine_metadata_keeps_raw_resolved_paths_not_shell_expressions() {
        let path = "/home/user's files/$literal/shepr";
        let resolved = RemoteShepr::new(path);
        assert_eq!(
            resolved
                .machine_metadata()
                .expect("test precondition")
                .executable,
            path
        );
        assert_eq!(resolved.quoted(), shell_quote(path));
    }

    #[test]
    fn remote_output_framing_discards_any_banner_and_preserves_binary() {
        let payload = [0, 1, 2, 0xff, b'\n'];
        let mut input = vec![b'x'; 4 * 1024 * 1024];
        input.extend_from_slice(b"\r\nshepr-remote-output-ready:1\r\n");
        input.extend_from_slice(&payload);
        let mut reader = io::BufReader::with_capacity(17, io::Cursor::new(input));

        discard_remote_output_preamble(&mut reader).expect("test precondition");
        let mut output = Vec::new();
        io::Read::read_to_end(&mut reader, &mut output).expect("test precondition");
        assert_eq!(output, payload);

        let mut missing = b"profile output without marker".to_vec();
        assert!(normalize_remote_stdout(&mut missing, true).is_err());
        normalize_remote_stdout(&mut missing, false).expect("test precondition");
        assert_eq!(missing, b"profile output without marker");

        let mut framed = b"profile output\nshepr-remote-output-ready:1\nhello\n".to_vec();
        normalize_remote_stdout(&mut framed, true).expect("test precondition");
        assert_eq!(framed, b"hello\n");
    }

    #[test]
    fn reattach_command_includes_remote_and_session() {
        assert_eq!(
            reattach_command(
                "target/release/shepr",
                "user@host",
                "work",
                RemoteKeybindings::Local,
            ),
            "target/release/shepr --remote user@host --session work"
        );
        assert_eq!(
            reattach_command(
                "shepr",
                "host name",
                crate::session::DEFAULT_SESSION_NAME,
                RemoteKeybindings::Local,
            ),
            "shepr --remote 'host name'"
        );
        assert_eq!(
            reattach_command(
                "shepr",
                "host",
                crate::session::DEFAULT_SESSION_NAME,
                RemoteKeybindings::Server,
            ),
            "shepr --remote host --remote-keybindings server"
        );
    }

    #[test]
    fn noninteractive_remote_bridge_requests_idle_timeout() {
        let remote = RemoteShepr::new("/usr/bin/shepr");
        assert!(
            remote
                .bridge_command("agents", true)
                .ends_with(" --session agents remote-client-bridge --idle-timeout-v1'")
        );
    }

    #[test]
    fn remote_bridge_command_uses_installed_binary() {
        let remote_shepr = RemoteShepr::new("/usr/bin/shepr");
        assert_eq!(
            remote_shepr.bridge_command(crate::session::DEFAULT_SESSION_NAME, false),
            "/bin/sh -c 'echo; echo shepr-remote-output-ready:1; exec /usr/bin/shepr remote-client-bridge'"
        );
        assert_eq!(
            remote_shepr.saved_bridge_command("agents"),
            "exec /usr/bin/shepr --session agents remote-client-bridge </dev/null"
        );
    }

    #[test]
    fn remote_path_discovery_uses_path_binary() {
        let remote_shepr =
            remote_shepr_from_path_discovery("/usr/bin/shepr\n").expect("path binary");

        assert_eq!(
            remote_shepr.bridge_command(crate::session::DEFAULT_SESSION_NAME, false),
            "/bin/sh -c 'echo; echo shepr-remote-output-ready:1; exec /usr/bin/shepr remote-client-bridge'"
        );
    }

    #[test]
    fn remote_path_discovery_quotes_discovered_binary() {
        let remote_shepr =
            remote_shepr_from_path_discovery("/opt/shepr bin/shepr\n").expect("path binary");

        assert_eq!(
            remote_shepr.bridge_command(crate::session::DEFAULT_SESSION_NAME, false),
            "/bin/sh -c 'echo; echo shepr-remote-output-ready:1; exec '\\''/opt/shepr bin/shepr'\\'' remote-client-bridge'"
        );
    }

    /// The bridge command is interpreted by /bin/sh, not by the login shell: the
    /// login shell only sees `/bin/sh -c` and one quoted word without newlines.
    #[test]
    fn saved_bridge_command_does_not_depend_on_a_posix_login_shell() {
        let command = RemoteShepr::new("/usr/bin/shepr").bridge_command("agents", true);
        let script = command
            .strip_prefix("/bin/sh -c '")
            .and_then(|rest| rest.strip_suffix('\''))
            .expect("wrapped in /bin/sh -c");
        assert!(!script.contains('\''), "{script}");
        assert!(!script.contains('\n'), "{script}");

        // The script, run by a real /bin/sh, still frames its output with the marker.
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(posix_remote_output_command("printf payload"))
            .output()
            .expect("test precondition");
        let mut stdout = output.stdout;
        normalize_remote_stdout(&mut stdout, output.status.success()).expect("marker line present");
        assert_eq!(stdout, b"payload");
    }

    #[test]
    fn remote_path_discovery_reads_multiple_absolute_paths() {
        let candidates =
            remote_sheprs_from_path_discovery("/usr/bin/shepr\nbin/shepr\n /opt/shepr bin/shepr\n");

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].path, "/usr/bin/shepr");
        assert_eq!(candidates[1].path, "/opt/shepr bin/shepr");
    }

    #[test]
    fn remote_path_discovery_ignores_mise_shims() {
        let candidates = remote_sheprs_from_path_discovery(
            "/home/can/.local/share/mise/shims/shepr\n/home/can/.local/share/mise/installs/shepr/0.7.1/bin/shepr\n",
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].path,
            "/home/can/.local/share/mise/installs/shepr/0.7.1/bin/shepr"
        );
    }

    #[test]
    fn known_remote_binary_candidate_script_includes_cargo_and_local_bin() {
        let script = known_remote_binary_candidate_script();

        assert!(script.contains("emit \"$home/.cargo/bin/shepr\""));
        assert!(script.contains("emit \"$home/.local/bin/shepr\""));
    }

    #[test]
    fn remote_path_discovery_quotes_single_quotes_in_discovered_binary() {
        let remote_shepr =
            remote_shepr_from_path_discovery("/opt/shepr's/bin/shepr\n").expect("path binary");

        assert_eq!(
            remote_shepr.bridge_command(crate::session::DEFAULT_SESSION_NAME, false),
            posix_shell_command(
                "echo; echo shepr-remote-output-ready:1; exec '/opt/shepr'\\''s/bin/shepr' remote-client-bridge"
            )
        );
    }

    #[test]
    fn remote_path_discovery_ignores_relative_paths() {
        let remote_shepr = remote_shepr_from_path_discovery("bin/shepr\n");

        assert!(remote_shepr.is_none());
    }

    #[test]
    fn remote_path_discovery_ignores_empty_output() {
        let remote_shepr = remote_shepr_from_path_discovery("\n");

        assert!(remote_shepr.is_none());
    }

    #[test]
    fn parse_client_status_json_reads_last_json_record() {
        let status = parse_client_status_json(
            "wrapper output\n{\"version\":\"0.8.0\",\"protocol\":20}\n{\"wrapper\":true}\n",
        )
        .expect("test precondition");
        assert_eq!(status.version.as_deref(), Some("0.8.0"));
        assert_eq!(status.protocol, Some(20));
    }

    #[test]
    fn parse_remote_server_status_json_reads_running_server() {
        assert_eq!(
            parse_remote_server_status_json(
                r#"{"status":"running","running":true,"version":"0.6.0","protocol":8,"capabilities":{"detached_server_daemon":true,"surface_interest":true,"health_check":true,"ssh_agent_registration":false}}"#
            )
            .expect("test precondition"),
            RemoteServerStatus::Running {
                version: Some("0.6.0".into()),
                protocol: Some(8),
                surface_interest: true,
                health_check: true,
                detached_server_daemon: true
            }
        );
    }

    #[test]
    fn parse_remote_server_status_json_reads_stopped_server() {
        assert_eq!(
            parse_remote_server_status_json(
                r#"{"status":"not_running","running":false,"version":null,"protocol":null}"#
            )
            .expect("test precondition"),
            RemoteServerStatus::NotRunning
        );
    }

    fn remote_env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn socket_path_byte_len(path: &Path) -> usize {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().len()
    }

    #[test]
    fn local_forward_socket_path_uses_readable_name_when_it_fits() {
        let _guard = remote_env_lock().lock().expect("test precondition");
        // Short target + session leave plenty of room - keep the human-
        // readable form so the socket path stays grep-friendly.
        let path = local_forward_socket_path("dev", "default");
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        assert!(
            filename.starts_with("shepr-remote-"),
            "expected readable name, got {filename}"
        );
        assert!(filename.contains("-dev-default."), "got {filename}");
        assert!(
            fits_unix_socket_path(&path),
            "socket path too long: {} ({} bytes)",
            path.display(),
            socket_path_byte_len(&path)
        );
    }

    #[test]
    fn local_forward_socket_path_fits_in_sun_path() {
        let _guard = remote_env_lock().lock().expect("test precondition");
        // Worst case for the readable form: a 49-char TMPDIR +
        // max-length sanitized components. Should fall back to the hashed
        // short name, which fits under TMPDIR.
        let target = "longish-host.example.com";
        let session = "a-fairly-long-session-name-here";
        let path = local_forward_socket_path(target, session);
        assert!(
            fits_unix_socket_path(&path),
            "socket path too long for sun_path: {} ({} bytes)",
            path.display(),
            socket_path_byte_len(&path)
        );
    }

    #[test]
    fn local_forward_socket_path_falls_back_to_tmp_when_dir_is_long() {
        let _guard = remote_env_lock().lock().expect("test precondition");
        // Force a TMPDIR long enough that even the hashed short name cannot
        // fit inside it. The fallback should drop to /tmp.
        let prior = std::env::var_os("TMPDIR");
        let long_dir = std::env::temp_dir().join("a".repeat(80));
        let _ = fs::create_dir_all(&long_dir);
        unsafe { std::env::set_var("TMPDIR", &long_dir) };

        let path = local_forward_socket_path("longish-host.example.com", "default");
        let fits = fits_unix_socket_path(&path);
        let parent = path.parent().map(Path::to_path_buf);
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        match prior {
            Some(v) => unsafe { std::env::set_var("TMPDIR", v) },
            None => unsafe { std::env::remove_var("TMPDIR") },
        }
        let _ = fs::remove_dir_all(&long_dir);

        assert!(fits, "fallback path still overflows: {}", path.display());
        assert_eq!(parent.as_deref(), Some(Path::new("/tmp")));
        assert!(
            filename.starts_with("shepr-r-"),
            "expected hashed fallback, got {filename}"
        );
    }
}
