use std::io;
use std::path::PathBuf;

use crate::machine::{ProfileId, RemoteExecutable, SshMetadataCache, SshTarget};

use super::{
    DiscoveryProgress, RemoteSsh, SshStdioBridge, resume_installed_remote_shepr_discovery,
};

pub struct SavedSshBridge {
    bridge: SshStdioBridge,
}

impl SavedSshBridge {
    /// The SSH failure behind a connection that closed early, if the bridge reported one
    /// (waits briefly for the bridge thread). SSH stderr otherwise only reaches the log,
    /// and the caller would see a bare end of stream.
    pub fn reported_failure(&self) -> Option<io::Error> {
        self.bridge.reported_failure()
    }
}

pub struct SavedSshStream {
    pub stream: shepr_platform::ipc::LocalStream,
    pub bridge: SavedSshBridge,
}

/// Settings a saved-machine connector takes from the config. A client reads its
/// config once at launch and hands these in, so reconnects never re-read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavedSshSettings {
    pub manage_ssh_config: bool,
}

/// Connects one saved SSH machine repeatedly while it remains in the catalog with the same
/// target and session (the client follows catalog edits and builds a new connector when a
/// machine is removed, re-added or re-pointed).
///
/// It owns what used to be rebuilt on every attempt: the ssh settings fixed at
/// launch, one temporary managed ssh config (instead of a new directory per
/// attempt), and the remote executable found by the last successful discovery.
/// Discovery costs several SSH round trips (a login-shell `command -v`, a `/bin/sh`
/// `command -v`, the candidate script, a status probe per candidate), so a
/// reconnect launches the bridge straight from the remembered executable, seeded
/// from the on-disk metadata cache at first use.
///
/// A remembered executable is only a hint. When an attempt with it fails for any
/// reason other than the SSH link itself, the hint is dropped and the same attempt
/// runs full discovery once more, so a moved, removed or upgraded remote install
/// costs one extra bridge launch and never a stuck endpoint.
///
/// Full discovery may not fit in one attempt on a slow link without connection
/// sharing. When an attempt ends on a timeout or other link failure, what discovery
/// completed is kept (`DiscoveryProgress`) and the next attempt
/// resumes it, so every attempt still ends within its budget and discovery still
/// finishes.
pub struct SavedSshConnector {
    paths: shepr_config::AppPaths,
    profile_id: ProfileId,
    target: SshTarget,
    session: String,
    settings: SavedSshSettings,
    state: std::sync::Mutex<ConnectorState>,
}

#[derive(Default)]
struct ConnectorState {
    ssh: Option<RemoteSsh>,
    launch_fatal_setup_error: Option<StoredSetupError>,
    remote_shepr: Option<RemoteExecutable>,
    /// Full discovery's completed round trips, while it has not finished. Only kept while
    /// there is no remembered executable.
    discovery: DiscoveryProgress,
    seeded_from_disk: bool,
}

#[derive(Clone)]
struct StoredSetupError {
    kind: io::ErrorKind,
    message: String,
}

impl StoredSetupError {
    fn capture(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    fn to_io_error(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}

impl SavedSshConnector {
    pub fn new(
        paths: &shepr_config::AppPaths,
        profile_id: &ProfileId,
        target: &SshTarget,
        session: &str,
        settings: SavedSshSettings,
    ) -> Self {
        let connector = Self {
            paths: paths.clone(),
            profile_id: profile_id.clone(),
            target: target.clone(),
            session: session.to_owned(),
            settings,
            state: std::sync::Mutex::new(ConnectorState::default()),
        };
        connector.prepare_for_launch();
        connector
    }

    /// Reports a deterministic local setup failure found while constructing this saved
    /// connector. The client checks it before entering its retry loop; transient filesystem
    /// failures remain in the connector and are tried again by `connect`.
    pub fn launch_fatal_setup_error(&self) -> Option<io::Error> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .launch_fatal_setup_error
            .as_ref()
            .map(StoredSetupError::to_io_error)
    }

    fn prepare_for_launch(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(error) = self.validate_local_setup() {
            if is_launch_fatal_setup_error(&error) {
                state.launch_fatal_setup_error = Some(StoredSetupError::capture(&error));
            } else {
                tracing::debug!(%error, "saved SSH path setup failed transiently; it will be retried");
            }
            return;
        }
        match RemoteSsh::new_noninteractive_with(
            self.target.clone(),
            self.settings.manage_ssh_config,
            &self.paths,
        ) {
            Ok(ssh) => state.ssh = Some(ssh),
            Err(error) if is_launch_fatal_setup_error(&error) => {
                state.launch_fatal_setup_error = Some(StoredSetupError::capture(&error));
            }
            Err(error) => {
                tracing::debug!(%error, "saved SSH setup failed transiently; it will be retried");
            }
        }
    }

    fn validate_local_setup(&self) -> io::Result<()> {
        shepr_api::session::validate_name(&self.session)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        // This path is needed even when managed SSH config is disabled. Validate it at
        // launch so an XDG_RUNTIME_DIR that can never hold the local bridge socket fails
        // before the endpoint's first scheduled connection attempt.
        saved_bridge_path(self.paths.xdg_runtime_dir(), &self.profile_id)?;
        if self.settings.manage_ssh_config {
            shepr_platform::shared_ssh_control_path(
                self.paths.xdg_runtime_dir(),
                self.paths.config_file(),
                self.target.as_str(),
            )?;
        }
        Ok(())
    }

    /// Starts a bridge and hands its stream to `establish`, which runs the endpoint
    /// handshake. The handshake is part of the attempt so that a failure there can
    /// still send the attempt back through discovery.
    ///
    /// Nothing SSH runs past `deadline`: discovery commands are cut short by it and no
    /// step starts once it has passed (the caller holds `establish` to it too). Without
    /// it, discovery and a remembered-executable retry could add up to minutes against
    /// a host that hangs, and the next attempt waits for this one.
    pub fn connect<T>(
        &self,
        deadline: std::time::Instant,
        mut establish: impl FnMut(SavedSshStream) -> io::Result<T>,
    ) -> io::Result<T> {
        shepr_api::session::validate_name(&self.session)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let target = &self.target;
        let metadata_cache = SshMetadataCache::new(
            &self.paths,
            &self.profile_id,
            target.as_str(),
            &self.session,
        );
        // The client stores this connector in a cloneable ConnectTarget::Ssh(Arc<...>) and
        // clones that target into its blocking task, so connect updates state through shared
        // access. Its supervisor starts only one attempt per endpoint and defers replacements
        // until retired attempts report, so calls in that path do not contend. Removing this
        // lock would require moving connector ownership into the task and returning it with
        // the attempt result.
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(error) = &state.launch_fatal_setup_error {
            return Err(error.to_io_error());
        }
        if !state.seeded_from_disk {
            state.seeded_from_disk = true;
            state.remote_shepr = metadata_cache.load();
        }
        if state.ssh.is_none() {
            // A transient local runtime-directory or managed-config failure must not
            // disable this endpoint for the connector's lifetime. Failed setup leaves
            // `ssh` empty, so the next scheduled connection attempt tries it again.
            state.ssh = Some(RemoteSsh::new_noninteractive_with(
                self.target.clone(),
                self.settings.manage_ssh_config,
                &self.paths,
            )?);
        }
        let ConnectorState {
            ssh,
            remote_shepr,
            discovery,
            ..
        } = &mut *state;
        // Setup above either stored the transport or returned its setup error. Keep
        // this checked arm instead of panicking if the connector state changes later.
        let Some(ssh) = ssh.as_mut() else {
            return Err(io::Error::other("saved SSH transport is unavailable"));
        };
        ssh.set_attempt_deadline(Some(deadline));
        let ssh = &*ssh;

        if let Some(known) = remote_shepr.clone() {
            match self.attempt(ssh, target, &known, deadline, &mut establish) {
                Ok(connected) => return Ok(connected),
                Err(error) if super::is_ssh_link_failure(&error) => return Err(error),
                Err(error) => {
                    tracing::debug!(
                        %error,
                        "remembered remote Shepr did not connect; rediscovering"
                    );
                    *remote_shepr = None;
                    // This hint cache is shared with API bridges. A failed connection or
                    // handshake only invalidates this connector's in-memory hint; an API
                    // bridge removes the shared entry only after its explicit stale marker.
                }
            }
        }

        let discovered =
            resume_installed_remote_shepr_discovery(ssh, discovery).inspect_err(|error| {
                if discovery.has_progress() {
                    tracing::debug!(%error, "SSH discovery stopped; the next attempt resumes it");
                }
            })?;
        *discovery = DiscoveryProgress::default();
        // Remembered before the bridge starts: when only the bridge runs out of time or
        // loses the link, the next attempt launches it straight away instead of
        // discovering again. Any other failure forgets it, so the next attempt discovers
        // from scratch.
        *remote_shepr = Some(discovered.clone());
        match self.attempt(ssh, target, &discovered, deadline, &mut establish) {
            Ok(connected) => {
                // The connection is up and this connector keeps the hint in memory;
                // only later processes and reconnects after a restart lose it.
                if let Err(error) = metadata_cache.store(&discovered) {
                    tracing::warn!(
                        %error,
                        profile = %self.profile_id,
                        target = %target.as_str(),
                        path = %metadata_cache.path().display(),
                        "could not cache SSH machine metadata; later connections rediscover the remote shepr"
                    );
                }
                Ok(connected)
            }
            Err(error) => {
                if !super::is_ssh_link_failure(&error) {
                    *remote_shepr = None;
                }
                Err(error)
            }
        }
    }

    fn attempt<T>(
        &self,
        ssh: &RemoteSsh,
        target: &SshTarget,
        remote_shepr: &RemoteExecutable,
        deadline: std::time::Instant,
        establish: &mut impl FnMut(SavedSshStream) -> io::Result<T>,
    ) -> io::Result<T> {
        if std::time::Instant::now() >= deadline {
            return Err(super::attempt_deadline_passed());
        }
        let path = saved_bridge_path(self.paths.xdg_runtime_dir(), &self.profile_id)?;
        let bridge = SshStdioBridge::start(
            target.clone(),
            remote_shepr,
            path.clone(),
            &self.session,
            ssh.options(),
            true,
        )?;
        let stream = shepr_platform::ipc::connect_local_stream(&path)?;
        establish(SavedSshStream {
            stream,
            bridge: SavedSshBridge { bridge },
        })
    }
}

pub struct SavedSshApiBridge {
    path: PathBuf,
    bridge: SshStdioBridge,
    metadata_cache: SshMetadataCache,
    pub used_cached_metadata: bool,
    metadata_store_failure: Option<io::Error>,
}

impl SavedSshApiBridge {
    pub fn start(
        paths: &shepr_config::AppPaths,
        profile_id: &ProfileId,
        target: &SshTarget,
        session: &str,
        use_cached_metadata: bool,
        settings: SavedSshSettings,
    ) -> io::Result<Self> {
        let ssh = validated_saved_ssh(paths, target, session, settings)?;
        let metadata_cache = SshMetadataCache::new(paths, profile_id, target.as_str(), session);
        let cached = use_cached_metadata.then(|| metadata_cache.load()).flatten();
        let used_cached_metadata = cached.is_some();
        let mut metadata_store_failure = None;
        let metadata = match cached {
            Some(metadata) => metadata,
            None => {
                let metadata = super::discover_remote_api_executable(&ssh, session)?;
                // Discovery succeeded, so this command can proceed; a failed store
                // only costs every later command another discovery. It is kept for
                // the caller to report: the CLI process that starts this bridge has
                // no log subscriber, so a log line here would reach nobody.
                metadata_store_failure = metadata_cache.store(&metadata).err();
                metadata
            }
        };
        let command = super::cached_remote_api_command(&metadata, session);
        // The managed SSH config remains necessary on a cache hit: its include and
        // ControlMaster options are still applied by the bridge's SSH subprocess.
        let path = shepr_platform::remote_bridge_endpoint_path(
            paths.xdg_runtime_dir(),
            &format!("shepr-api-ssh-{}-{profile_id}.sock", std::process::id()),
            &format!(
                "shepr-api-{}-{}.sock",
                std::process::id(),
                &profile_id.as_str()[..16]
            ),
        )?;
        let bridge = SshStdioBridge::start_command(
            target.clone(),
            command,
            path.clone(),
            ssh.options(),
            true,
        )?;
        Ok(Self {
            path,
            bridge,
            metadata_cache,
            used_cached_metadata,
            metadata_store_failure,
        })
    }

    /// Why the discovered executable could not be cached, when this bridge
    /// discovered it and the store failed.
    pub fn metadata_store_failure(&self) -> Option<&io::Error> {
        self.metadata_store_failure.as_ref()
    }

    pub fn socket_path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn reported_failure(&self) -> Option<io::Error> {
        self.bridge.reported_failure()
    }

    /// Removes the shared hint this bridge started from. A failure leaves the stale
    /// hint for the next command, which then pays one failed attempt before it
    /// rediscovers; the caller decides whether to report it.
    pub fn invalidate_metadata(&self) -> io::Result<()> {
        self.metadata_cache.invalidate()
    }

    /// The metadata cache file, for naming it when invalidating fails.
    pub fn metadata_path(&self) -> &std::path::Path {
        self.metadata_cache.path()
    }

    pub fn stale_metadata_failure(error: &io::Error) -> bool {
        super::SshFailureDiagnostic::from_error(error).is_stale_metadata()
    }
}

pub fn saved_ssh_bootstrap_command(target: &str, session: &str) -> String {
    format!(
        "shepr --remote {} --session {}",
        super::shell_quote(target),
        super::shell_quote(session)
    )
}

fn saved_bridge_path(runtime_dir: &std::path::Path, profile_id: &ProfileId) -> io::Result<PathBuf> {
    let pid = std::process::id();
    let readable = format!("shepr-ssh-{pid}-{profile_id}.sock");
    let short = format!("shepr-s-{pid}-{}.sock", &profile_id.as_str()[..16]);
    shepr_platform::remote_bridge_endpoint_path(runtime_dir, &readable, &short)
}

fn is_launch_fatal_setup_error(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::InvalidInput {
        return true;
    }
    // A policy violation cannot recover on retry, while an OS permission error can.
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<shepr_platform::UnsafeSshRuntimeDirectory>())
        .is_some()
}

fn validated_saved_ssh(
    paths: &shepr_config::AppPaths,
    target: &SshTarget,
    session: &str,
    settings: SavedSshSettings,
) -> io::Result<RemoteSsh> {
    shepr_api::session::validate_name(session)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    RemoteSsh::new_noninteractive_with(target.clone(), settings.manage_ssh_config, paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_fixtures::AppPathsFixture as _;

    #[test]
    fn only_typed_runtime_directory_policy_errors_are_launch_fatal() {
        let policy = io::Error::new(
            io::ErrorKind::PermissionDenied,
            shepr_platform::UnsafeSshRuntimeDirectory,
        );
        assert!(is_launch_fatal_setup_error(&policy));
        let ordinary = io::Error::new(io::ErrorKind::PermissionDenied, policy.to_string());
        assert!(!is_launch_fatal_setup_error(&ordinary));
        assert!(is_launch_fatal_setup_error(&io::Error::from(
            io::ErrorKind::InvalidInput,
        )));
    }

    #[test]
    fn bridge_paths_use_profile_identity_not_target_or_session() {
        let runtime_dir = shepr_test_support::ScratchDir::new("saved-bridge-paths");
        let first = saved_bridge_path(
            runtime_dir.path(),
            &ProfileId::parse("0123456789abcdef0123456789abcdef").expect("test precondition"),
        )
        .expect("test precondition");
        let second = saved_bridge_path(
            runtime_dir.path(),
            &ProfileId::parse("fedcba9876543210fedcba9876543210").expect("test precondition"),
        )
        .expect("test precondition");
        assert_ne!(first, second);
        assert!(!first.to_string_lossy().contains("example.com"));
        assert!(!first.to_string_lossy().contains("default"));
    }

    #[test]
    fn connector_rejects_invalid_session_before_touching_ssh() {
        let settings = SavedSshSettings {
            manage_ssh_config: false,
        };
        let connector = SavedSshConnector::new(
            &shepr_config::AppPaths::test_default(),
            &ProfileId::parse("0123456789abcdef0123456789abcdef").expect("test precondition"),
            &SshTarget::parse("build").expect("test precondition"),
            "bad session/name",
            settings,
        );
        let error = connector
            .connect(
                std::time::Instant::now() + std::time::Duration::from_secs(30),
                |_| -> io::Result<()> { panic!("no attempt may start") },
            )
            .expect_err("test precondition");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn bootstrap_command_preserves_the_explicit_remote_session() {
        assert_eq!(
            saved_ssh_bootstrap_command("build host", "agent work"),
            "shepr --remote 'build host' --session 'agent work'"
        );
    }

    #[test]
    fn prompt_and_compatibility_failures_require_attention() {
        let authentication = super::super::SshFailureDiagnostic::from_ssh_output(
            Some(super::super::SSH_OWN_FAILURE_EXIT_CODE),
            "Permission denied (publickey)".into(),
        );
        assert!(
            super::super::SshFailureDiagnostic::from_error(&io::Error::other(authentication))
                .needs_attention()
        );
        let host_key = super::super::SshFailureDiagnostic::from_ssh_output(
            Some(super::super::SSH_OWN_FAILURE_EXIT_CODE),
            "Host key verification failed".into(),
        );
        assert!(
            super::super::SshFailureDiagnostic::from_error(&io::Error::other(host_key))
                .needs_attention()
        );
        for error in [
            io::Error::new(
                io::ErrorKind::Unsupported,
                "matching Shepr is not ready; install or update",
            ),
            io::Error::new(io::ErrorKind::InvalidData, "handshake rejected"),
        ] {
            assert!(super::super::SshFailureDiagnostic::from_error(&error).needs_attention());
        }
        assert!(
            !super::super::SshFailureDiagnostic::from_error(&io::Error::new(
                io::ErrorKind::TimedOut,
                "network timed out"
            ))
            .needs_attention()
        );
        assert!(
            !super::super::SshFailureDiagnostic::from_error(&io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "server shut down during handshake"
            ))
            .needs_attention()
        );
        for message in [
            "Protocol mismatch in unrelated SSH stderr",
            "remote command mentioned protocol in its output",
        ] {
            assert!(
                !super::super::SshFailureDiagnostic::from_error(&io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    message,
                ))
                .needs_attention()
            );
        }
    }
}
