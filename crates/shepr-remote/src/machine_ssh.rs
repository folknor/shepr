use std::io;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};

use shepr_launch::{EndpointFailure, FailureCause};

use crate::bridge::{SshStdioBridge, ssh_bridge_exit_error, ssh_bridge_exit_error_after_session};
use crate::discovery::{
    DiscoveryProgress, resume_installed_remote_shepr_discovery, verify_remote_shepr,
};
use crate::failure::{attempt_deadline_passed, failure_evidence, local_setup_error};
use crate::host::BridgeMode;
use crate::limits::{PIPE_DRAIN_GRACE, SERVER_WATCH_POLL_INTERVAL, SSH_STDERR_CAPTURE_LIMIT};
use crate::machine::{MachineLabel, RemoteExecutable, SshMetadataCache, SshTarget};
use crate::process::{PipeCapture, kill_and_reap};
use crate::server_lifecycle::{remote_server_status, stop_server_of_another_build};
use crate::ssh::{RemoteSsh, SshMode, ssh_invocation};

/// Rebuilds `ssh`'s managed config when there is none or its file has gone
/// (a removed temporary directory while the client stayed open), keeping a
/// transport whose config still exists.
fn ensure_managed_ssh(
    ssh: &mut Option<RemoteSsh>,
    target: &SshTarget,
    paths: &shepr_paths::AppPaths,
    deadline: std::time::Instant,
) -> io::Result<()> {
    let must_rebuild = match ssh.as_ref() {
        Some(ssh) => !ssh.options().config_path.try_exists()?,
        None => true,
    };
    if must_rebuild {
        *ssh = Some(RemoteSsh::new(target.clone(), paths, deadline)?);
    }
    Ok(())
}

/// One machine's executable resolution and preflight server validation. Disk metadata is an
/// untrusted hint until the installed client and sibling have been verified. A
/// verified hint is reused during this process; only evidence about that executable
/// invalidates it. Discovery itself owns the rule for retaining completed round
/// trips across transient network failures and authentication waits, and clearing them
/// after SSH process failures such as authentication rejection or host-key errors.
#[derive(Default)]
pub(crate) struct MachineProbe {
    ssh: Option<RemoteSsh>,
    executable: ProbeExecutable,
    discovery: DiscoveryProgress,
}

#[derive(Default)]
enum ProbeExecutable {
    #[default]
    Unseeded,
    Missing,
    Hint(RemoteExecutable),
    Verified(RemoteExecutable),
}

impl MachineProbe {
    /// The startup check: resolves the remote executable and reads the
    /// server's status through it, without starting or judging anything. It
    /// exists to find out, before the client takes the terminal, whether the
    /// machine needs an authentication prompt.
    pub(crate) fn check(
        &mut self,
        paths: &shepr_paths::AppPaths,
        target: &SshTarget,
        deadline: std::time::Instant,
    ) -> io::Result<()> {
        ensure_managed_ssh(&mut self.ssh, target, paths, deadline)?;
        let Some(mut ssh) = self.ssh.take() else {
            return Err(io::Error::other("machine SSH transport is unavailable"));
        };
        ssh.set_attempt_deadline(deadline);
        let cache = SshMetadataCache::new(paths, target);
        let result = self.advance(&ssh, &cache).map(|_| ());
        self.ssh = Some(ssh);
        result
    }

    fn advance(
        &mut self,
        ssh: &RemoteSsh,
        cache: &SshMetadataCache,
    ) -> io::Result<RemoteExecutable> {
        self.advance_with(
            cache,
            |candidate| verify_remote_shepr(ssh, candidate),
            |progress| resume_installed_remote_shepr_discovery(ssh, progress),
            |remote| remote_server_status(ssh, remote).map(|_| ()),
        )
    }

    fn resolve_remote(
        &mut self,
        ssh: &RemoteSsh,
        cache: &SshMetadataCache,
    ) -> io::Result<RemoteExecutable> {
        self.resolve(
            cache,
            |candidate| verify_remote_shepr(ssh, candidate),
            |progress| resume_installed_remote_shepr_discovery(ssh, progress),
        )
    }

    fn has_verified_executable(&self) -> bool {
        matches!(&self.executable, ProbeExecutable::Verified(_))
    }

    /// The IO seam keeps preflight resolution and server-judgment failures in one
    /// state machine, and lets tests supply remote results without SSH.
    fn advance_with(
        &mut self,
        cache: &SshMetadataCache,
        verify: impl FnMut(&RemoteExecutable) -> io::Result<bool>,
        discover: impl FnOnce(&mut DiscoveryProgress) -> io::Result<RemoteExecutable>,
        read_status: impl FnOnce(&RemoteExecutable) -> io::Result<()>,
    ) -> io::Result<RemoteExecutable> {
        let remote = self.resolve(cache, verify, discover)?;
        let result = read_status(&remote);
        if let Err(error) = &result {
            self.observe_failure(cache, error);
        }
        result.map(|()| remote)
    }

    fn resolve(
        &mut self,
        cache: &SshMetadataCache,
        mut verify: impl FnMut(&RemoteExecutable) -> io::Result<bool>,
        discover: impl FnOnce(&mut DiscoveryProgress) -> io::Result<RemoteExecutable>,
    ) -> io::Result<RemoteExecutable> {
        if matches!(self.executable, ProbeExecutable::Unseeded) {
            self.executable = cache
                .load()
                .map_or(ProbeExecutable::Missing, ProbeExecutable::Hint);
        }
        match &self.executable {
            ProbeExecutable::Verified(remote) => return Ok(remote.clone()),
            ProbeExecutable::Hint(cached) => {
                let cached = cached.clone();
                match verify(&cached) {
                    Ok(true) => {
                        self.executable = ProbeExecutable::Verified(cached.clone());
                        return Ok(cached);
                    }
                    // Keep the hint only when verification learned nothing
                    // about the candidate or could not trust the target. Once
                    // that path ran and answered with a fault or mismatch, let
                    // discovery try the remaining candidates as it does for a
                    // newly discovered path.
                    Err(error) if !failure_evidence(&error).rejects_candidate() => {
                        return Err(error);
                    }
                    Ok(false) | Err(_) => self.invalidate(cache),
                }
            }
            ProbeExecutable::Unseeded | ProbeExecutable::Missing => {}
        }
        let discovered = discover(&mut self.discovery).inspect_err(|error| {
            if self.discovery.has_progress() {
                tracing::debug!(
                    %error,
                    path = %cache.path().display(),
                    "SSH discovery stopped; the next attempt resumes it"
                );
            }
        })?;
        self.discovery = DiscoveryProgress::default();
        self.executable = ProbeExecutable::Verified(discovered.clone());
        if let Err(error) = cache.store(&discovered) {
            shepr_platform::structured_log!(
                WARN, event = remote.metadata_save, outcome = Error,
                %error,
                path = %cache.path().display(),
                "could not cache SSH machine metadata; later connections rediscover the remote shepr"
            );
        }
        Ok(discovered)
    }

    fn invalidate(&mut self, cache: &SshMetadataCache) {
        self.executable = ProbeExecutable::Missing;
        self.discovery = DiscoveryProgress::default();
        if let Err(error) = cache.invalidate() {
            shepr_platform::structured_log!(
                WARN, event = remote.metadata_invalidate, outcome = Error,
                %error,
                path = %cache.path().display(),
                "could not drop stale SSH machine metadata"
            );
        }
    }

    fn observe_failure(&mut self, cache: &SshMetadataCache, error: &io::Error) -> bool {
        if failure_evidence(error).invalidates_executable() {
            self.invalidate(cache);
            true
        } else {
            false
        }
    }
}

pub struct MachineSshBridge {
    bridge: SshStdioBridge,
}

impl MachineSshBridge {
    /// The SSH failure behind a connection that closed early, if the bridge reported one
    /// (joins its connection worker after EOF). SSH stderr otherwise only reaches the log,
    /// and the caller would see a bare end of stream.
    pub fn reported_failure(&self) -> Option<io::Error> {
        self.bridge.reported_failure()
    }
}

pub struct MachineSshStream {
    pub stream: shepr_platform::ipc::LocalStream,
    pub bridge: MachineSshBridge,
}

/// What one connection attempt to a configured machine may do to its server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectMode {
    /// Attach to a server that is running. Every connection the client makes
    /// by itself is one: it never starts a server, and a host with none
    /// fails the attempt with a no-server failure.
    Attach,
    /// The operator's Connect: start the server when none runs, then attach.
    Start,
    /// The operator's Restart: stop the running server of another build (only
    /// the boot that was observed), then start this build's and attach.
    Restart,
}

impl ConnectMode {
    fn bridge_mode(self) -> BridgeMode {
        match self {
            Self::Attach => BridgeMode::Attach,
            Self::Start | Self::Restart => BridgeMode::Start,
        }
    }
}

/// How a wait for a machine's server ended without failing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerWatchEnd {
    /// The remote wait exited: a server answers there now, or the wait ran
    /// its longest life. Either way the next step is an attaching attempt.
    Ended,
    /// The client cancelled the wait.
    Cancelled,
}

/// Connects one configured SSH machine repeatedly. The machine set is fixed at
/// launch, so a connector lives as long as its client.
///
/// It keeps the managed SSH config and its own `MachineProbe` for executable
/// resolution. Startup preflight uses the same resolution and reads the
/// server's status; a background connection starts the bridge directly and lets
/// the bridge and the handshake report a missing server or one of another
/// build. A verified executable is reused on reconnect, so an ordinary
/// reconnect needs one SSH bridge round trip. Partial discovery survives
/// transient network failures and authentication waits so a slow host can be
/// resolved over several bounded attempts.
pub struct MachineSshConnector {
    paths: shepr_paths::AppPaths,
    label: MachineLabel,
    target: SshTarget,
    state: ConnectorState,
}

#[derive(Default)]
struct ConnectorState {
    ssh: Option<RemoteSsh>,
    launch_fatal_setup_error: Option<StoredSetupError>,
    probe: MachineProbe,
}

#[derive(Clone)]
struct StoredSetupError {
    kind: io::ErrorKind,
    failure: EndpointFailure,
}

impl StoredSetupError {
    fn capture(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            failure: EndpointFailure::from_error(error),
        }
    }

    fn to_io_error(&self) -> io::Error {
        io::Error::new(self.kind, self.failure.clone())
    }
}

impl MachineSshConnector {
    pub fn new(paths: &shepr_paths::AppPaths, label: &MachineLabel, target: &SshTarget) -> Self {
        let mut connector = Self {
            paths: paths.clone(),
            label: label.clone(),
            target: target.clone(),
            state: ConnectorState::default(),
        };
        connector.prepare_for_launch();
        connector
    }

    pub(crate) fn from_preflight(
        paths: &shepr_paths::AppPaths,
        machine: &shepr_config::MachineConfig,
        mut probe: MachineProbe,
    ) -> Self {
        let ssh = probe.ssh.take();
        let mut connector = Self {
            paths: paths.clone(),
            label: machine.label.clone(),
            target: machine.ssh.clone(),
            state: ConnectorState {
                ssh,
                probe,
                launch_fatal_setup_error: None,
            },
        };
        if connector.state.ssh.is_none() {
            connector.prepare_for_launch();
        }
        connector
    }

    pub fn label(&self) -> &MachineLabel {
        &self.label
    }

    /// Reports a deterministic local setup failure found while constructing this
    /// connector. The client checks it before entering its retry loop; transient filesystem
    /// failures remain in the connector and are tried again by `connect`.
    pub fn launch_fatal_setup_error(&self) -> Option<io::Error> {
        self.state
            .launch_fatal_setup_error
            .as_ref()
            .map(StoredSetupError::to_io_error)
    }

    fn prepare_for_launch(&mut self) {
        // clock-io-ok: launch validates config only; each attempt replaces this expired deadline.
        match RemoteSsh::new(self.target.clone(), &self.paths, std::time::Instant::now()) {
            Ok(ssh) => self.state.ssh = Some(ssh),
            Err(error) if is_launch_fatal_setup_error(&error) => {
                self.state.launch_fatal_setup_error = Some(StoredSetupError::capture(&error));
            }
            Err(error) => {
                shepr_platform::structured_log!(
                    WARN, event = remote.ssh_setup, outcome = Retry,
                    %error,
                    machine = %self.label,
                    target = %self.target,
                    "machine SSH setup failed transiently; it will be retried"
                );
            }
        }
    }

    /// The machine's transport, set up when it is missing or its config file
    /// went away, with `deadline` bounding the bounded commands it runs next,
    /// and the probe that resolves its executable. A launch-fatal setup error
    /// is returned every time.
    fn transport(
        &mut self,
        deadline: std::time::Instant,
    ) -> io::Result<(&RemoteSsh, &mut MachineProbe)> {
        let state = &mut self.state;
        if let Some(error) = &state.launch_fatal_setup_error {
            return Err(error.to_io_error());
        }
        // A missing managed config can mean its temporary directory was removed
        // while the client stayed open. Rebuild it as local setup rather than retrying
        // ssh with a path that no longer exists. If logind removed the XDG runtime
        // root itself, the resulting NotFound is shown as Attention and retried;
        // setup rebuilds once that root returns.
        ensure_managed_ssh(&mut state.ssh, &self.target, &self.paths, deadline)?;
        let ConnectorState { ssh, probe, .. } = &mut *state;
        // Setup above either stored the transport or returned its setup error. Keep
        // this checked arm instead of panicking if the connector state changes later.
        let Some(ssh) = ssh.as_mut() else {
            return Err(io::Error::other("machine SSH transport is unavailable"));
        };
        ssh.set_attempt_deadline(deadline);
        Ok((&*ssh, probe))
    }

    /// Starts a bridge in the mode `mode` allows and hands its stream to
    /// `establish`, which runs the endpoint handshake. Exclusive access keeps all
    /// mutable connection state owned by the one supervisor attempt using this
    /// connector. The handshake result is returned as-is unless the remote
    /// command failed to execute the remembered path. A [`ConnectMode::Restart`]
    /// first stops a running server of another build, by the boot identity
    /// its status reports.
    ///
    /// Discovery commands use the smaller of their command timeout and the time left,
    /// and refuse to start once `deadline` has passed. The stdio bridge does not receive
    /// this deadline. Callers must bound `establish` separately; bridge teardown follows
    /// its stream and stop signals. Without the discovery limit, multiple round trips could
    /// add up to minutes against a host that hangs, and the next attempt waits for this one.
    pub fn connect<T>(
        &mut self,
        deadline: std::time::Instant,
        mode: ConnectMode,
        mut establish: impl FnMut(MachineSshStream) -> io::Result<T>,
    ) -> io::Result<T> {
        let metadata_cache = SshMetadataCache::new(&self.paths, &self.target);
        let target = self.target.clone();
        let (ssh, probe) = self.transport(deadline)?;

        let remote = probe.resolve_remote(ssh, &metadata_cache)?;
        let established_session = probe.has_verified_executable();
        if mode == ConnectMode::Restart {
            stop_server_of_another_build(ssh, &remote).inspect_err(|error| {
                probe.observe_failure(&metadata_cache, error);
            })?;
        }
        let bridge_mode = mode.bridge_mode();
        match Self::attempt(
            ssh,
            &target,
            &remote,
            bridge_mode,
            established_session,
            deadline,
            &mut establish,
        ) {
            Ok(connected) => Ok(connected),
            Err(error) if probe.observe_failure(&metadata_cache, &error) => {
                // The remote command proved the path stale after probing. Resolve
                // once more within the same deadline, through the same state machine.
                let remote = probe.resolve_remote(ssh, &metadata_cache)?;
                let established_session = probe.has_verified_executable();
                Self::attempt(
                    ssh,
                    &target,
                    &remote,
                    bridge_mode,
                    established_session,
                    deadline,
                    &mut establish,
                )
                .inspect_err(|error| {
                    probe.observe_failure(&metadata_cache, error);
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Runs the remote wait for a server over the machine's shared control
    /// connection and blocks until it exits or `cancel` is set. It starts
    /// nothing on the machine. `deadline` bounds only resolving the remote
    /// executable; the wait itself runs as long as no server appears. The
    /// wait's ssh is kept on an open stdin, whose close is how the remote side
    /// learns this client went away.
    pub fn wait_for_server(
        &mut self,
        deadline: std::time::Instant,
        cancel: &AtomicBool,
    ) -> io::Result<ServerWatchEnd> {
        let metadata_cache = SshMetadataCache::new(&self.paths, &self.target);
        let target = self.target.clone();
        let (ssh, probe) = self.transport(deadline)?;
        let remote = probe.resolve_remote(ssh, &metadata_cache)?;
        let established_session = probe.has_verified_executable();
        let mut command = ssh_invocation(&target, ssh.options(), SshMode::Batch);
        command
            .arg(remote.wait_for_server_command().as_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| local_setup_error("could not start local ssh", error))?;
        // Held open for the life of the wait; dropped with the child at the end.
        let _stdin = child.stdin.take();
        let stderr = child
            .stderr
            .take()
            .map(|stderr| PipeCapture::spawn(stderr, SSH_STDERR_CAPTURE_LIMIT));
        let status = loop {
            if cancel.load(Ordering::Acquire) {
                kill_and_reap(&mut child, "ssh server wait");
                return Ok(ServerWatchEnd::Cancelled);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(SERVER_WATCH_POLL_INTERVAL),
                Err(error) => {
                    kill_and_reap(&mut child, "ssh server wait");
                    return Err(error);
                }
            }
        };
        if status.success() {
            return Ok(ServerWatchEnd::Ended);
        }
        // Bounded: a ControlPersist master forked by this ssh can hold its
        // stderr open long after the wait itself exited.
        let stderr = match stderr {
            Some(capture) => capture.finish(PIPE_DRAIN_GRACE)?,
            None => Vec::new(),
        };
        let error = if established_session {
            ssh_bridge_exit_error_after_session(status, &stderr)
        } else {
            ssh_bridge_exit_error(status, &stderr)
        };
        probe.observe_failure(&metadata_cache, &error);
        Err(error)
    }

    fn attempt<T>(
        ssh: &RemoteSsh,
        target: &SshTarget,
        remote_shepr: &RemoteExecutable,
        mode: BridgeMode,
        established_session: bool,
        deadline: std::time::Instant,
        establish: &mut impl FnMut(MachineSshStream) -> io::Result<T>,
    ) -> io::Result<T> {
        // clock-io-ok: discovery and bridge setup may have consumed the attempt budget.
        if std::time::Instant::now() >= deadline {
            return Err(attempt_deadline_passed());
        }
        let (bridge, stream) = SshStdioBridge::start(
            target.clone(),
            remote_shepr,
            mode,
            established_session,
            ssh.options(),
        )
        .map_err(|error| local_setup_error("could not start local SSH bridge", error))?;
        // clock-io-ok: starting the bridge spent real time; establishment has its own bound.
        if std::time::Instant::now() >= deadline {
            return Err(attempt_deadline_passed());
        }
        establish(MachineSshStream {
            stream,
            bridge: MachineSshBridge { bridge },
        })
    }
}

fn is_launch_fatal_setup_error(error: &io::Error) -> bool {
    // Whether a local setup failure can never succeed on retry. Only the
    // shared control socket path and ssh config setup are classified here; discovery
    // errors never reach it. This is launch admission, not the endpoint attention
    // policy: an actionable filesystem failure may still recover while the client
    // runs, whereas an impossible path must reject launch before taking the terminal.
    if let Some(failure) = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<EndpointFailure>())
    {
        return failure.cause() == FailureCause::InvalidLocalSetup;
    }
    // Raw invalid input, such as an impossible config path, is permanent.
    error.kind() == io::ErrorKind::InvalidInput
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::DiscoverySteps;
    use crate::failure::{
        SSH_OWN_FAILURE_EXIT_CODE, SshFailureDiagnostic, remote_candidate_mismatch_error,
        remote_compatibility_error,
    };

    fn remote_executable_must_be_rediscovered(error: &io::Error) -> bool {
        failure_evidence(error).invalidates_executable()
    }

    fn failure_needs_attention(error: &io::Error) -> bool {
        EndpointFailure::from_error(error)
            .disposition()
            .needs_attention()
    }

    #[test]
    fn launch_setup_input_and_runtime_policy_errors_are_fatal() {
        let policy =
            crate::failure::ssh_runtime_error(crate::ssh_paths::SshRuntimeError::UnsafeDirectory(
                crate::ssh_paths::UnsafeSshRuntimeDirectory::new(std::path::Path::new("/runtime")),
            ));
        assert!(is_launch_fatal_setup_error(&policy));
        let ordinary = io::Error::new(io::ErrorKind::PermissionDenied, policy.to_string());
        assert!(!is_launch_fatal_setup_error(&ordinary));
        assert!(is_launch_fatal_setup_error(&io::Error::from(
            io::ErrorKind::InvalidInput,
        )));
    }

    #[test]
    fn a_vanished_xdg_runtime_root_is_a_retryable_attention_failure() {
        // A successful rebuild needs a runtime root as short as a real
        // `/run/user/<uid>` for the SSH control socket name, which no scratch
        // directory is; this covers how the missing root is classified.
        let scratch = shepr_test_support::ScratchDir::new("machine-runtime-root-recovery");
        let xdg_runtime = scratch.join("xdg-runtime");
        let paths = shepr_paths::AppPaths::rooted_at(&xdg_runtime, None, None)
            .expect("scratch roots fit a socket");
        let target = SshTarget::parse("build.example").expect("test precondition");
        let mut ssh = None;

        let error = ensure_managed_ssh(&mut ssh, &target, &paths, std::time::Instant::now())
            .expect_err("missing runtime root prevents SSH setup");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            !is_launch_fatal_setup_error(&error),
            "a runtime root that can return remains retryable: {error}"
        );
        assert!(
            failure_evidence(&error).preserves_discovery(),
            "a missing local runtime root establishes no remote install evidence"
        );
        assert!(
            failure_needs_attention(&error),
            "a missing local runtime root is actionable, not a dropped SSH link"
        );
        assert!(ssh.is_none(), "a failed setup keeps nothing to reuse");
    }

    #[test]
    fn connector_reuses_the_executable_verified_by_preflight() {
        let scratch = shepr_test_support::ScratchDir::new("preflight-connector");
        let paths = shepr_paths::AppPaths::rooted_at(&scratch, Some(&scratch), None)
            .expect("scratch roots fit a socket");
        let machine = shepr_config::MachineConfig {
            label: MachineLabel::parse("build").expect("test label"),
            ssh: SshTarget::parse("build.example").expect("test target"),
            palette: shepr_config::DEFAULT_LOCAL_HUE,
        };
        let executable = executable("/usr/bin/shepr");
        let probe = MachineProbe {
            executable: ProbeExecutable::Verified(executable.clone()),
            ..MachineProbe::default()
        };
        let mut connector = MachineSshConnector::from_preflight(&paths, &machine, probe);
        let cache = SshMetadataCache::new(&paths, &machine.ssh);
        let resolved = connector
            .state
            .probe
            .resolve(
                &cache,
                |_| panic!("preflight already verified this executable"),
                |_| panic!("preflight already discovered this executable"),
            )
            .expect("verified executable is retained");
        assert_eq!(resolved, executable);
        assert_eq!(connector.label(), &machine.label);
    }

    fn resolve_remote_shepr(
        cache: &SshMetadataCache,
        verify: impl FnMut(&RemoteExecutable) -> io::Result<bool>,
        discover: impl FnOnce() -> io::Result<RemoteExecutable>,
    ) -> io::Result<RemoteExecutable> {
        MachineProbe::default().resolve(cache, verify, |_| discover())
    }

    fn cache_in(scratch: &shepr_test_support::ScratchDir) -> SshMetadataCache {
        let paths = shepr_paths::AppPaths::rooted_at(scratch, None, None)
            .expect("scratch roots fit a socket");
        SshMetadataCache::new(
            &paths,
            &SshTarget::parse("build.example").expect("test precondition"),
        )
    }

    fn executable(path: &str) -> RemoteExecutable {
        RemoteExecutable::parse(path).expect("test precondition")
    }

    #[test]
    fn typed_install_evidence_invalidates_a_remembered_path() {
        for exit_code in [126, 127] {
            let diagnostic = SshFailureDiagnostic::from_ssh_output(
                Some(exit_code),
                &format!("remote command failed (exit status {exit_code})"),
            );
            let error = io::Error::other(diagnostic);
            assert!(remote_executable_must_be_rediscovered(&error));
        }

        // The text alone proves nothing: only ssh's typed report of the remote
        // command's exit status does.
        let quoted = io::Error::other("remote command failed (exit status 127)");
        assert!(!remote_executable_must_be_rediscovered(&quoted));
        let own_failure = io::Error::other(SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "ssh: connect to host h port 22: Connection refused",
        ));
        assert!(!remote_executable_must_be_rediscovered(&own_failure));

        // Only a typed incompatibility from the protocol boundary needs
        // attention; a bare Unsupported IO kind no longer does.
        let server_mismatch = io::Error::other(EndpointFailure::different_build(
            "SSH endpoint handshake failed: build mismatch: peer is a different shepr build",
        ));
        assert!(!remote_executable_must_be_rediscovered(&server_mismatch));
        assert!(failure_needs_attention(&server_mismatch));
    }

    #[test]
    fn a_verified_cached_executable_skips_discovery() {
        let scratch = shepr_test_support::ScratchDir::new("machine-check-cached");
        let cache = cache_in(&scratch);
        cache
            .store(&executable("/cached/shepr"))
            .expect("test precondition");
        let found = resolve_remote_shepr(
            &cache,
            |_| Ok(true),
            || panic!("a verified cache entry needs no discovery"),
        )
        .expect("resolved");
        assert_eq!(found, executable("/cached/shepr"));
    }

    #[test]
    fn a_verified_executable_skips_ssh_resolution_on_reconnect() {
        let scratch = shepr_test_support::ScratchDir::new("machine-check-reconnect");
        let cache = cache_in(&scratch);
        cache
            .store(&executable("/cached/shepr"))
            .expect("test precondition");
        let mut probe = MachineProbe::default();
        let first = probe
            .resolve(&cache, |_| Ok(true), |_| panic!("verified cache"))
            .expect("first connection verifies the cached executable");
        let reconnect = probe
            .resolve(
                &cache,
                |_| panic!("a reconnect reuses the verified executable"),
                |_| panic!("a reconnect does not rediscover the executable"),
            )
            .expect("reconnect reuses the verified executable");
        assert_eq!(reconnect, first);
    }

    #[test]
    fn a_stale_cached_executable_is_dropped_and_discovery_is_recorded() {
        let scratch = shepr_test_support::ScratchDir::new("machine-check-stale");
        let cache = cache_in(&scratch);
        cache
            .store(&executable("/old/shepr"))
            .expect("test precondition");
        let found = resolve_remote_shepr(&cache, |_| Ok(false), || Ok(executable("/new/shepr")))
            .expect("resolved");
        assert_eq!(found, executable("/new/shepr"));
        assert_eq!(cache.load(), Some(executable("/new/shepr")));

        // A probe that ran and answered with an untyped fault rejects the hint
        // too: discovery tries the remaining candidates.
        let found = resolve_remote_shepr(
            &cache,
            |_| Err(io::Error::new(io::ErrorKind::Unsupported, "another build")),
            || Ok(executable("/newer/shepr")),
        )
        .expect("an answered fault falls through to discovery");
        assert_eq!(found, executable("/newer/shepr"));
        assert_eq!(cache.load(), Some(executable("/newer/shepr")));
    }

    #[test]
    fn a_failure_before_a_remote_result_keeps_the_cache_and_skips_discovery() {
        let scratch = shepr_test_support::ScratchDir::new("machine-check-no-remote-result");
        let cache = cache_in(&scratch);
        cache
            .store(&executable("/cached/shepr"))
            .expect("test precondition");
        let error = resolve_remote_shepr(
            &cache,
            |_| Err(io::Error::from(io::ErrorKind::TimedOut)),
            || panic!("no remote result says nothing about the cached path"),
        )
        .expect_err("no remote result");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(cache.load(), Some(executable("/cached/shepr")));
    }

    #[test]
    fn discovery_without_a_cache_entry_is_recorded() {
        let scratch = shepr_test_support::ScratchDir::new("machine-check-empty");
        let cache = cache_in(&scratch);
        let found = resolve_remote_shepr(
            &cache,
            |_| panic!("nothing cached to verify"),
            || Ok(executable("/found/shepr")),
        )
        .expect("resolved");
        assert_eq!(found, executable("/found/shepr"));
        assert_eq!(cache.load(), Some(executable("/found/shepr")));
    }

    #[test]
    fn preflight_reuses_verified_discovery_after_a_server_probe_failure() {
        let scratch = shepr_test_support::ScratchDir::new("machine-probe-server-link");
        let cache = cache_in(&scratch);
        let mut probe = MachineProbe::default();
        let error = probe
            .advance_with(
                &cache,
                |_| panic!("empty cache"),
                |_| Ok(executable("/found/shepr")),
                |_| Err(io::Error::from(io::ErrorKind::TimedOut)),
            )
            .expect_err("server status timed out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(cache.load(), Some(executable("/found/shepr")));
        let remote = probe
            .advance_with(
                &cache,
                |_| panic!("already verified"),
                |_| panic!("already discovered"),
                |_| Ok(()),
            )
            .expect("next preflight check only probes server presence");
        assert_eq!(remote, executable("/found/shepr"));
    }

    #[test]
    fn a_remote_exec_failure_drops_memory_and_disk_then_rediscovers() {
        let scratch = shepr_test_support::ScratchDir::new("machine-probe-stale-memory");
        let cache = cache_in(&scratch);
        cache
            .store(&executable("/old/shepr"))
            .expect("test precondition");
        let mut probe = MachineProbe::default();
        probe
            .resolve(&cache, |_| Ok(true), |_| panic!("verified cache"))
            .expect("verified");
        let error = io::Error::other(SshFailureDiagnostic::from_ssh_output(
            Some(127),
            "remote executable disappeared",
        ));
        assert!(probe.observe_failure(&cache, &error));
        assert!(cache.load().is_none());
        let found = probe
            .resolve(
                &cache,
                |_| panic!("stale hint was dropped"),
                |_| Ok(executable("/new/shepr")),
            )
            .expect("rediscovered");
        assert_eq!(found, executable("/new/shepr"));
        assert_eq!(cache.load(), Some(found));
    }

    #[test]
    fn cached_candidate_mismatch_rediscovery_uses_the_shared_typed_class() {
        let scratch = shepr_test_support::ScratchDir::new("machine-probe-candidate-build");
        let cache = cache_in(&scratch);
        cache
            .store(&executable("/old/shepr"))
            .expect("test precondition");
        let mut probe = MachineProbe::default();
        let found = probe
            .resolve(
                &cache,
                |_| Err(remote_candidate_mismatch_error("installed pair changed")),
                |_| {
                    assert!(cache.load().is_none(), "stale cache is removed first");
                    Ok(executable("/new/shepr"))
                },
            )
            .expect("a different installed pair triggers discovery");
        assert_eq!(cache.load(), Some(found));
    }

    #[test]
    fn a_hint_that_ran_but_failed_verification_falls_through_to_discovery() {
        let failures = [
            (
                "remote-fault",
                io::Error::other(SshFailureDiagnostic::from_ssh_output(
                    Some(1),
                    "remote client status probe failed: exit status 1",
                )),
            ),
            (
                "install-changed",
                remote_compatibility_error("remote client status returned invalid JSON"),
            ),
        ];

        for (label, error) in failures {
            let scratch = shepr_test_support::ScratchDir::new(label);
            let cache = cache_in(&scratch);
            cache
                .store(&executable("/cached/shepr"))
                .expect("test precondition");
            let mut probe = MachineProbe::default();
            let mut verification_error = Some(error);
            let found = probe
                .resolve(
                    &cache,
                    |_| {
                        Err(verification_error
                            .take()
                            .expect("the cached hint is verified once"))
                    },
                    |_| {
                        assert!(cache.load().is_none(), "rejected hint is invalidated first");
                        Ok(executable("/discovered/shepr"))
                    },
                )
                .expect("a failed hint verification falls through to discovery");
            assert_eq!(found, executable("/discovered/shepr"));
            assert_eq!(cache.load(), Some(found));
        }
    }

    #[test]
    fn a_preflight_server_status_exec_failure_invalidates_the_candidate() {
        let scratch = shepr_test_support::ScratchDir::new("machine-probe-status-exec");
        let cache = cache_in(&scratch);
        let mut probe = MachineProbe::default();
        let error = probe
            .advance_with(
                &cache,
                |_| panic!("empty cache"),
                |_| Ok(executable("/found/shepr")),
                |_| {
                    Err(io::Error::other(SshFailureDiagnostic::from_ssh_output(
                        Some(126),
                        "remote executable no longer executable",
                    )))
                },
            )
            .expect_err("executable changed between discovery and server status");
        assert!(remote_executable_must_be_rediscovered(&error));
        assert!(cache.load().is_none());
        assert!(matches!(probe.executable, ProbeExecutable::Missing));
    }

    #[test]
    fn ssh_rejections_leave_the_disk_hint_untrusted_and_available_for_a_recheck() {
        for message in [
            "Permission denied (publickey)",
            "Host key verification failed",
        ] {
            let scratch = shepr_test_support::ScratchDir::new("machine-probe-hint-rejection");
            let cache = cache_in(&scratch);
            cache
                .store(&executable("/cached/shepr"))
                .expect("test precondition");
            let mut probe = MachineProbe::default();
            let error = probe
                .resolve(
                    &cache,
                    |_| {
                        Err(io::Error::other(SshFailureDiagnostic::from_ssh_output(
                            Some(SSH_OWN_FAILURE_EXIT_CODE),
                            message,
                        )))
                    },
                    |_| panic!("no remote result means no discovery fallback"),
                )
                .expect_err("SSH rejection");
            assert!(failure_needs_attention(&error));
            assert!(matches!(probe.executable, ProbeExecutable::Hint(_)));
            assert_eq!(cache.load(), Some(executable("/cached/shepr")));
            let found = probe
                .resolve(
                    &cache,
                    |_| Ok(true),
                    |_| panic!("hint verified after SSH recovers"),
                )
                .expect("recheck verifies the hint");
            assert_eq!(found, executable("/cached/shepr"));
        }
    }

    struct InterruptedDiscovery {
        account_calls: usize,
        interruption: Option<io::Error>,
        target: SshTarget,
    }

    impl DiscoverySteps for InterruptedDiscovery {
        fn path_via_account_shell(&mut self) -> io::Result<Option<RemoteExecutable>> {
            self.account_calls += 1;
            Ok(Some(executable("/found/shepr")))
        }

        fn known_locations(&mut self) -> io::Result<Vec<RemoteExecutable>> {
            if let Some(error) = self.interruption.take() {
                Err(error)
            } else {
                Ok(Vec::new())
            }
        }

        fn matches(&mut self, _: &RemoteExecutable) -> io::Result<bool> {
            Ok(true)
        }

        fn target(&self) -> &SshTarget {
            &self.target
        }
    }

    #[test]
    fn machine_probe_resumes_discovery_without_a_remote_result_unless_the_target_is_untrusted() {
        let failures = [
            (io::Error::from(io::ErrorKind::TimedOut), 1),
            (
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    SshFailureDiagnostic::authentication_wait_timeout(),
                ),
                1,
            ),
            (
                io::Error::other(SshFailureDiagnostic::from_ssh_output(
                    Some(SSH_OWN_FAILURE_EXIT_CODE),
                    "Permission denied (publickey)",
                )),
                1,
            ),
            (
                io::Error::other(SshFailureDiagnostic::from_ssh_output(
                    Some(SSH_OWN_FAILURE_EXIT_CODE),
                    "Host key verification failed",
                )),
                // The target's identity is in doubt, so discovery starts over.
                2,
            ),
        ];
        for (index, (failure, expected_account_calls)) in failures.into_iter().enumerate() {
            let scratch =
                shepr_test_support::ScratchDir::new(&format!("machine-probe-progress-{index}"));
            let cache = cache_in(&scratch);
            let mut probe = MachineProbe::default();
            let mut discovery = InterruptedDiscovery {
                account_calls: 0,
                interruption: Some(failure),
                target: SshTarget::parse("build.example").expect("test precondition"),
            };
            assert!(
                probe
                    .resolve(
                        &cache,
                        |_| panic!("empty cache"),
                        |progress| progress.advance(&mut discovery),
                    )
                    .is_err()
            );
            let found = probe
                .resolve(
                    &cache,
                    |_| panic!("no completed discovery to verify"),
                    |progress| progress.advance(&mut discovery),
                )
                .expect("next round completes discovery");
            assert_eq!(found, executable("/found/shepr"));
            assert_eq!(discovery.account_calls, expected_account_calls);
            assert!(!probe.discovery.has_progress());
        }
    }

    /// Only the operator's Connect and Restart run a bridge that may start the
    /// machine's server; every automatic attempt attaches.
    #[test]
    fn only_the_operator_modes_start_a_server() {
        assert_eq!(ConnectMode::Attach.bridge_mode(), BridgeMode::Attach);
        assert_eq!(ConnectMode::Start.bridge_mode(), BridgeMode::Start);
        assert_eq!(ConnectMode::Restart.bridge_mode(), BridgeMode::Start);
    }

    #[test]
    fn prompt_and_compatibility_failures_require_attention() {
        let authentication = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "Permission denied (publickey)",
        );
        let authentication = io::Error::other(authentication);
        assert!(failure_needs_attention(&authentication));
        let host_key = SshFailureDiagnostic::from_ssh_output(
            Some(SSH_OWN_FAILURE_EXIT_CODE),
            "Host key verification failed",
        );
        let host_key = io::Error::other(host_key);
        assert!(failure_needs_attention(&host_key));
        let compatibility =
            remote_compatibility_error("matching Shepr is not ready; install or update");
        let compatibility_failure = EndpointFailure::from_error(&compatibility);
        assert!(compatibility_failure.disposition().needs_attention());
        assert_eq!(
            compatibility_failure.disposition(),
            shepr_launch::FailureDisposition::Incompatible
        );
        for kind in [
            io::ErrorKind::InvalidInput,
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
        ] {
            let error = local_setup_error(
                "local setup failure",
                io::Error::new(kind, "the local operation failed"),
            );
            let failure = EndpointFailure::from_error(&error);
            assert!(
                matches!(
                    failure.cause(),
                    FailureCause::LocalSetup | FailureCause::InvalidLocalSetup
                ),
                "{kind}"
            );
            assert!(failure.disposition().needs_attention(), "{kind}");
        }
        for kind in [io::ErrorKind::InvalidData, io::ErrorKind::Unsupported] {
            let error = local_setup_error(
                "local setup failure",
                io::Error::new(kind, "the local operation failed"),
            );
            let failure = EndpointFailure::from_error(&error);
            assert!(
                matches!(
                    failure.cause(),
                    FailureCause::LocalSetup | FailureCause::InvalidLocalSetup
                ),
                "{kind}"
            );
            assert!(failure.disposition().needs_attention(), "{kind}");
        }
        let timed_out = io::Error::new(io::ErrorKind::TimedOut, "network timed out");
        assert!(!failure_needs_attention(&timed_out));
        let aborted = io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "server shut down during handshake",
        );
        assert!(!failure_needs_attention(&aborted));
        for message in [
            "Protocol mismatch in unrelated SSH stderr",
            "remote command mentioned protocol in its output",
        ] {
            let error = io::Error::new(io::ErrorKind::ConnectionAborted, message);
            assert!(!failure_needs_attention(&error));
        }
    }
}
