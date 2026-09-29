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
/// target (the client follows catalog edits and builds a new connector when a
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
    settings: SavedSshSettings,
    state: ConnectorState,
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
        settings: SavedSshSettings,
    ) -> Self {
        let mut connector = Self {
            paths: paths.clone(),
            profile_id: profile_id.clone(),
            target: target.clone(),
            settings,
            state: ConnectorState::default(),
        };
        connector.prepare_for_launch();
        connector
    }

    /// Reports a deterministic local setup failure found while constructing this saved
    /// connector. The client checks it before entering its retry loop; transient filesystem
    /// failures remain in the connector and are tried again by `connect`.
    pub fn launch_fatal_setup_error(&self) -> Option<io::Error> {
        self.state
            .launch_fatal_setup_error
            .as_ref()
            .map(StoredSetupError::to_io_error)
    }

    fn prepare_for_launch(&mut self) {
        if let Err(error) = self.validate_local_setup() {
            if is_launch_fatal_setup_error(&error) {
                self.state.launch_fatal_setup_error = Some(StoredSetupError::capture(&error));
            } else {
                tracing::warn!(
                    %error,
                    profile = %self.profile_id,
                    target = %self.target.as_str(),
                    "saved SSH path setup failed transiently; it will be retried"
                );
            }
            return;
        }
        match RemoteSsh::new_noninteractive_with(
            self.target.clone(),
            self.settings.manage_ssh_config,
            &self.paths,
        ) {
            Ok(ssh) => self.state.ssh = Some(ssh),
            Err(error) if is_launch_fatal_setup_error(&error) => {
                self.state.launch_fatal_setup_error = Some(StoredSetupError::capture(&error));
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    profile = %self.profile_id,
                    target = %self.target.as_str(),
                    "saved SSH setup failed transiently; it will be retried"
                );
            }
        }
    }

    fn validate_local_setup(&self) -> io::Result<()> {
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
    /// still send the attempt back through discovery. Exclusive access keeps all mutable
    /// connection state owned by the one supervisor attempt using this connector.
    ///
    /// Discovery commands use the smaller of their command timeout and the time left,
    /// and refuse to start once `deadline` has passed. The stdio bridge does not receive
    /// this deadline. Callers must bound `establish` separately; bridge teardown follows
    /// its stream and stop signals. Without the discovery limit, multiple round trips could
    /// add up to minutes against a host that hangs, and the next attempt waits for this one.
    pub fn connect<T>(
        &mut self,
        deadline: std::time::Instant,
        mut establish: impl FnMut(SavedSshStream) -> io::Result<T>,
    ) -> io::Result<T> {
        let target = &self.target;
        let metadata_cache = SshMetadataCache::new(&self.paths, &self.profile_id, target.as_str());
        let state = &mut self.state;
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
            match Self::attempt(
                &self.paths,
                &self.profile_id,
                ssh,
                target,
                &known,
                deadline,
                &mut establish,
            ) {
                Ok(connected) => return Ok(connected),
                Err(error) if super::is_ssh_link_failure(&error) => return Err(error),
                Err(error) => {
                    tracing::debug!(
                        %error,
                        profile = %self.profile_id,
                        target = %self.target.as_str(),
                        "remembered remote Shepr did not connect; rediscovering"
                    );
                    *remote_shepr = None;
                    // A failed connection or handshake only invalidates this
                    // connector's in-memory hint; the cached entry is overwritten
                    // once rediscovery succeeds.
                }
            }
        }

        let discovered =
            resume_installed_remote_shepr_discovery(ssh, discovery).inspect_err(|error| {
                if discovery.has_progress() {
                    tracing::debug!(
                        %error,
                        profile = %self.profile_id,
                        target = %self.target.as_str(),
                        "SSH discovery stopped; the next attempt resumes it"
                    );
                }
            })?;
        *discovery = DiscoveryProgress::default();
        // Remembered before the bridge starts: when only the bridge runs out of time or
        // loses the link, the next attempt launches it straight away instead of
        // discovering again. Any other failure forgets it, so the next attempt discovers
        // from scratch.
        *remote_shepr = Some(discovered.clone());
        match Self::attempt(
            &self.paths,
            &self.profile_id,
            ssh,
            target,
            &discovered,
            deadline,
            &mut establish,
        ) {
            Ok(connected) => {
                // A cache failure does not undo this connection or this connector's
                // in-memory hint; later processes must rediscover the executable.
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
        paths: &shepr_config::AppPaths,
        profile_id: &ProfileId,
        ssh: &RemoteSsh,
        target: &SshTarget,
        remote_shepr: &RemoteExecutable,
        deadline: std::time::Instant,
        establish: &mut impl FnMut(SavedSshStream) -> io::Result<T>,
    ) -> io::Result<T> {
        // clock-io-ok: discovery and bridge setup may have consumed the attempt budget.
        if std::time::Instant::now() >= deadline {
            return Err(super::attempt_deadline_passed());
        }
        let path = saved_bridge_path(paths.xdg_runtime_dir(), profile_id)?;
        let bridge =
            SshStdioBridge::start(target.clone(), remote_shepr, path.clone(), ssh.options())?;
        let stream = shepr_platform::ipc::connect_local_stream(&path)?;
        establish(SavedSshStream {
            stream,
            bridge: SavedSshBridge { bridge },
        })
    }
}

// The profile only makes these names readable; it is not what keeps bridges
// apart. `remote_bridge_endpoint_path` inserts a fresh random token into every
// name it hands out, so each bridge (every client attached to one saved
// machine, every connect attempt) binds a
// socket of its own and removes it on drop. Two bridges for one profile never
// contend for a path, so the busy-socket `AddrInUse` cannot arise between them.

/// A fresh socket path for one saved-machine attach bridge. The prefix is
/// distinct from the `shepr-ssh-` SSH config directories, whose sweep matches
/// on that prefix.
fn saved_bridge_path(runtime_dir: &std::path::Path, profile_id: &ProfileId) -> io::Result<PathBuf> {
    let readable = format!("shepr-bridge-{profile_id}.sock");
    let short = format!("shepr-b-{}.sock", profile_id.short());
    shepr_platform::remote_bridge_endpoint_path(runtime_dir, &readable, &short)
}

fn is_launch_fatal_setup_error(error: &io::Error) -> bool {
    // This launch-time classifier is only called for saved bridge
    // paths and SSH path setup. RemoteExecutable parsing happens during discovery after this
    // point and cannot reach it; those local InvalidInput failures are permanent setup errors.
    if error.kind() == io::ErrorKind::InvalidInput {
        return true;
    }
    // A policy violation cannot recover on retry, while an OS permission error can.
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<shepr_platform::UnsafeSshRuntimeDirectory>())
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_setup_input_and_runtime_policy_errors_are_fatal() {
        let policy = io::Error::new(
            io::ErrorKind::PermissionDenied,
            shepr_platform::UnsafeSshRuntimeDirectory::new(std::path::Path::new("/runtime")),
        );
        assert!(is_launch_fatal_setup_error(&policy));
        let ordinary = io::Error::new(io::ErrorKind::PermissionDenied, policy.to_string());
        assert!(!is_launch_fatal_setup_error(&ordinary));
        assert!(is_launch_fatal_setup_error(&io::Error::from(
            io::ErrorKind::InvalidInput,
        )));
    }

    #[test]
    fn bridge_paths_use_profile_identity_not_target() {
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

    /// Two clients attached to one saved machine each bind a bridge socket of
    /// their own at the same time, and dropping them leaves the runtime
    /// directory empty.
    #[test]
    fn concurrent_bridges_for_one_profile_each_bind_their_own_socket() {
        let runtime_dir = shepr_test_support::ScratchDir::new("saved-bridge-concurrent");
        let profile =
            ProfileId::parse("0123456789abcdef0123456789abcdef").expect("test precondition");
        let paths = [
            saved_bridge_path(runtime_dir.path(), &profile),
            saved_bridge_path(runtime_dir.path(), &profile),
        ]
        .map(|path| path.expect("test precondition"));
        let bridges: Vec<_> = paths
            .iter()
            .map(|path| {
                SshStdioBridge::start_command(
                    SshTarget::parse("example").expect("test precondition"),
                    "true".into(),
                    path.clone(),
                    None,
                )
                .expect("every concurrent bridge binds its own socket")
            })
            .collect();
        for (index, path) in paths.iter().enumerate() {
            assert!(path.starts_with(runtime_dir.path()), "{}", path.display());
            assert!(
                !paths[index + 1..].contains(path),
                "{} handed out twice",
                path.display()
            );
            // Not connected to: an accepted stream would start a real ssh.
            let metadata = std::fs::symlink_metadata(path).expect("bridge socket is bound");
            assert!(
                std::os::unix::fs::FileTypeExt::is_socket(&metadata.file_type()),
                "{}",
                path.display()
            );
        }

        drop(bridges);
        let left: Vec<_> = std::fs::read_dir(runtime_dir.path())
            .expect("test precondition")
            .map(|entry| entry.expect("test precondition").file_name())
            .collect();
        assert!(left.is_empty(), "bridges left files behind: {left:?}");
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
