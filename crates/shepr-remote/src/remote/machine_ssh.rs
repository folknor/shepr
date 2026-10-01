use std::io;
use std::path::PathBuf;

use crate::machine::{MachineLabel, RemoteExecutable, SshMetadataCache, SshTarget};

use super::{
    DiscoveryProgress, MachineSshCheck, RemoteSsh, SshStdioBridge, is_remote_candidate_mismatch,
    is_ssh_link_failure, judge_remote_server, locate_remote_shepr, remote_server_status,
    resume_installed_remote_shepr_discovery, verify_remote_shepr,
};

/// Checks, without prompting, that the configured machine can be served: SSH
/// works, a matching shepr and sibling `shepr-server` pair is found, and whether a
/// server already running there is this build. A stopped server is
/// [`MachineSshCheck::Ready`], since the bridge starts one on attach. A running
/// server of another build comes back as [`MachineSshCheck::DifferentBuild`] when it
/// can be restarted. No SSH command starts once `deadline` has passed, and each is
/// cut short at it.
///
/// The executable comes from the on-disk metadata cache when it is there and still
/// verifies (one round trip instead of full discovery). One that no longer verifies
/// is dropped, and full discovery runs; what discovery finds is recorded, so the
/// connector that follows does not repeat the round trips.
pub fn check_machine_ssh(
    paths: &shepr_config::AppPaths,
    target: &SshTarget,
    deadline: std::time::Instant,
) -> io::Result<MachineSshCheck> {
    let mut ssh = RemoteSsh::new(target.clone(), paths)?;
    ssh.set_attempt_deadline(Some(deadline));
    let cache = SshMetadataCache::new(paths, target);
    let remote = resolve_remote_shepr(
        &cache,
        |candidate| verify_remote_shepr(&ssh, candidate),
        || locate_remote_shepr(&ssh),
    )?;
    let status = remote_server_status(&ssh, &remote)?;
    judge_remote_server(ssh.target(), &remote, &status)
}

/// The remote executable for a check: the cached one when `verify` accepts it,
/// otherwise whatever `discover` finds, which is then cached. A link failure while
/// verifying says nothing about the cached path and is returned as it is. An absent
/// or incompatible cached executable is dropped before discovery; other probe errors
/// are returned without invalidating a path that may still be valid. Cache failures
/// are logged and never fail the check.
fn resolve_remote_shepr(
    cache: &SshMetadataCache,
    mut verify: impl FnMut(&RemoteExecutable) -> io::Result<bool>,
    discover: impl FnOnce() -> io::Result<RemoteExecutable>,
) -> io::Result<RemoteExecutable> {
    if let Some(cached) = cache.load() {
        match verify(&cached) {
            Ok(true) => return Ok(cached),
            Err(error) if is_ssh_link_failure(&error) => return Err(error),
            Err(error) if !is_remote_candidate_mismatch(&error) => return Err(error),
            Ok(false) | Err(_) => {
                if let Err(error) = cache.invalidate() {
                    tracing::warn!(
                        %error,
                        path = %cache.path().display(),
                        "could not drop stale SSH machine metadata"
                    );
                }
            }
        }
    }
    let discovered = discover()?;
    if let Err(error) = cache.store(&discovered) {
        tracing::warn!(
            %error,
            path = %cache.path().display(),
            "could not cache SSH machine metadata; later connections rediscover the remote shepr"
        );
    }
    Ok(discovered)
}

pub struct MachineSshBridge {
    bridge: SshStdioBridge,
}

impl MachineSshBridge {
    /// The SSH failure behind a connection that closed early, if the bridge reported one
    /// (waits briefly for the bridge thread). SSH stderr otherwise only reaches the log,
    /// and the caller would see a bare end of stream.
    pub fn reported_failure(&self) -> Option<io::Error> {
        self.bridge.reported_failure()
    }
}

pub struct MachineSshStream {
    pub stream: shepr_platform::ipc::LocalStream,
    pub bridge: MachineSshBridge,
}

/// Connects one configured SSH machine repeatedly. The machine set is fixed at
/// launch, so a connector lives as long as its client.
///
/// It owns what used to be rebuilt on every attempt: the ssh settings fixed at
/// launch, one temporary managed ssh config (instead of a new directory per
/// attempt), and the remote executable found by the last successful discovery.
/// Discovery costs several SSH round trips (a login-shell `command -v`, a `/bin/sh`
/// `command -v`, the candidate script, a status probe per candidate), so a
/// reconnect launches the bridge straight from the remembered executable, seeded
/// from the on-disk metadata cache at first use.
///
/// A remembered executable is only a hint. A remote command-not-found or
/// not-executable result drops it and runs discovery once; handshake and remote
/// launch errors leave the hint in place because they do not show that the
/// executable moved, was removed or was upgraded.
///
/// Full discovery may not fit in one attempt on a slow link without connection
/// sharing. When an attempt ends on a timeout or other link failure, what discovery
/// completed is kept (`DiscoveryProgress`) and the next attempt
/// resumes it, so every attempt still ends within its budget and discovery still
/// finishes.
pub struct MachineSshConnector {
    paths: shepr_config::AppPaths,
    label: MachineLabel,
    target: SshTarget,
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

fn ensure_managed_ssh_config(
    state: &mut ConnectorState,
    target: &SshTarget,
    paths: &shepr_config::AppPaths,
) -> io::Result<()> {
    let must_rebuild = match state.ssh.as_ref() {
        Some(ssh) => !ssh.options().config_path.try_exists()?,
        None => true,
    };
    if must_rebuild {
        state.ssh = Some(RemoteSsh::new(target.clone(), paths)?);
    }
    Ok(())
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
        crate::local_setup_error(
            "machine SSH setup failed",
            io::Error::new(self.kind, self.message.clone()),
        )
    }
}

impl MachineSshConnector {
    pub fn new(paths: &shepr_config::AppPaths, label: &MachineLabel, target: &SshTarget) -> Self {
        let mut connector = Self {
            paths: paths.clone(),
            label: label.clone(),
            target: target.clone(),
            state: ConnectorState::default(),
        };
        connector.prepare_for_launch();
        connector
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
        if let Err(error) = self.validate_local_setup() {
            if is_launch_fatal_setup_error(&error) {
                self.state.launch_fatal_setup_error = Some(StoredSetupError::capture(&error));
            } else {
                tracing::warn!(
                    %error,
                    machine = %self.label,
                    target = %self.target.as_str(),
                    "machine SSH path setup failed transiently; it will be retried"
                );
            }
            return;
        }
        match RemoteSsh::new(self.target.clone(), &self.paths) {
            Ok(ssh) => self.state.ssh = Some(ssh),
            Err(error) if is_launch_fatal_setup_error(&error) => {
                self.state.launch_fatal_setup_error = Some(StoredSetupError::capture(&error));
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    machine = %self.label,
                    target = %self.target.as_str(),
                    "machine SSH setup failed transiently; it will be retried"
                );
            }
        }
    }

    fn validate_local_setup(&self) -> io::Result<()> {
        // Validate both paths at launch so a runtime directory that can never hold the
        // local bridge socket or the shared control socket fails before the
        // endpoint's first scheduled connection attempt.
        let result = (|| {
            let runtime_dir = super::ssh::ensure_ssh_runtime_dir(&self.paths)?;
            validate_machine_bridge_path(runtime_dir, &self.label)?;
            shepr_platform::shared_ssh_control_path(
                runtime_dir,
                self.paths.config_file(),
                self.target.as_str(),
            )?;
            Ok(())
        })();
        result.map_err(|error| crate::local_setup_error("could not prepare local SSH paths", error))
    }

    /// Starts a bridge and hands its stream to `establish`, which runs the endpoint
    /// handshake. Exclusive access keeps all mutable connection state owned by the
    /// one supervisor attempt using this connector. The handshake result is returned
    /// as-is unless the remote command failed to execute the remembered path.
    ///
    /// Discovery commands use the smaller of their command timeout and the time left,
    /// and refuse to start once `deadline` has passed. The stdio bridge does not receive
    /// this deadline. Callers must bound `establish` separately; bridge teardown follows
    /// its stream and stop signals. Without the discovery limit, multiple round trips could
    /// add up to minutes against a host that hangs, and the next attempt waits for this one.
    pub fn connect<T>(
        &mut self,
        deadline: std::time::Instant,
        mut establish: impl FnMut(MachineSshStream) -> io::Result<T>,
    ) -> io::Result<T> {
        let target = &self.target;
        let metadata_cache = SshMetadataCache::new(&self.paths, target);
        let state = &mut self.state;
        if let Some(error) = &state.launch_fatal_setup_error {
            return Err(error.to_io_error());
        }
        if !state.seeded_from_disk {
            state.seeded_from_disk = true;
            state.remote_shepr = metadata_cache.load();
        }
        // A missing managed config can mean its temporary directory was removed
        // while the client stayed open. Rebuild it as local setup rather than retrying
        // ssh with a path that no longer exists. If logind removed the XDG runtime
        // root itself, the resulting NotFound is shown as Attention and retried;
        // setup rebuilds once that root returns.
        ensure_managed_ssh_config(state, &self.target, &self.paths)?;
        let ConnectorState {
            ssh,
            remote_shepr,
            discovery,
            ..
        } = &mut *state;
        // Setup above either stored the transport or returned its setup error. Keep
        // this checked arm instead of panicking if the connector state changes later.
        let Some(ssh) = ssh.as_mut() else {
            return Err(io::Error::other("machine SSH transport is unavailable"));
        };
        ssh.set_attempt_deadline(Some(deadline));
        let ssh = &*ssh;

        let discovered = if let Some(known) = remote_shepr.clone() {
            match Self::attempt(
                &self.paths,
                &self.label,
                ssh,
                target,
                &known,
                deadline,
                &mut establish,
            ) {
                Ok(connected) => return Ok(connected),
                Err(error) if remote_executable_must_be_rediscovered(&error) => {
                    tracing::debug!(
                        %error,
                        machine = %self.label,
                        target = %self.target.as_str(),
                        "remembered remote Shepr could not be executed; rediscovering"
                    );
                    *remote_shepr = None;
                    resume_installed_remote_shepr_discovery(ssh, discovery)?
                }
                Err(error) => return Err(error),
            }
        } else {
            resume_installed_remote_shepr_discovery(ssh, discovery).inspect_err(|error| {
                if discovery.has_progress() {
                    tracing::debug!(
                        %error,
                        machine = %self.label,
                        target = %self.target.as_str(),
                        "SSH discovery stopped; the next attempt resumes it"
                    );
                }
            })?
        };
        *discovery = DiscoveryProgress::default();
        // Remembered before the bridge starts: a link failure can retry this path
        // without rediscovery. Only a remote command-not-found or not-executable
        // result forgets it, so an unrelated server or launch failure does not pay
        // discovery again on the next attempt.
        *remote_shepr = Some(discovered.clone());
        match Self::attempt(
            &self.paths,
            &self.label,
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
                        machine = %self.label,
                        target = %target.as_str(),
                        path = %metadata_cache.path().display(),
                        "could not cache SSH machine metadata; later connections rediscover the remote shepr"
                    );
                }
                Ok(connected)
            }
            Err(error) => {
                if remote_executable_must_be_rediscovered(&error) {
                    *remote_shepr = None;
                }
                Err(error)
            }
        }
    }

    fn attempt<T>(
        paths: &shepr_config::AppPaths,
        label: &MachineLabel,
        ssh: &RemoteSsh,
        target: &SshTarget,
        remote_shepr: &RemoteExecutable,
        deadline: std::time::Instant,
        establish: &mut impl FnMut(MachineSshStream) -> io::Result<T>,
    ) -> io::Result<T> {
        // clock-io-ok: discovery and bridge setup may have consumed the attempt budget.
        if std::time::Instant::now() >= deadline {
            return Err(super::attempt_deadline_passed());
        }
        let path = machine_bridge_path(paths.runtime_dir(), label).map_err(|error| {
            crate::local_setup_error("could not prepare local SSH bridge", error)
        })?;
        let bridge = SshStdioBridge::start(
            target.clone(),
            remote_shepr,
            path.clone(),
            Some(ssh.options()),
        )
        .map_err(|error| {
            if shepr_platform::ipc::SocketBusy::from_io(&error).is_some() {
                error
            } else {
                crate::local_setup_error("could not start local SSH bridge", error)
            }
        })?;
        // clock-io-ok: starting the bridge spent real time; what is left bounds the connect.
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(super::attempt_deadline_passed());
        }
        let stream = shepr_platform::ipc::connect_trusted_local_stream_within(&path, remaining)?;
        establish(MachineSshStream {
            stream,
            bridge: MachineSshBridge { bridge },
        })
    }
}

/// Only the POSIX shell's command-not-found (127) and not-executable (126)
/// statuses, reported by ssh as the remote command's own exit status, prove
/// that a remembered Shepr path is stale. The status is read from the typed
/// diagnostic, never from message text, which carries remote stderr. A failed
/// handshake or remote launch error says nothing about that path. The bridge
/// relays bytes from the remote server socket, so a preamble build mismatch
/// identifies that server, not the Shepr executable that opened the bridge.
fn remote_executable_must_be_rediscovered(error: &io::Error) -> bool {
    matches!(
        super::SshFailureDiagnostic::from_error(error).remote_exit_code(),
        Some(126 | 127)
    )
}

// The label only makes these names readable; it is not what keeps bridges
// apart. `remote_bridge_endpoint_path` inserts a fresh random token into every
// name it hands out, so each bridge (every client attached to one configured
// machine, every connect attempt) binds a
// socket of its own and removes it on drop. Two bridges for one machine never
// contend for a path, so the busy-socket `AddrInUse` cannot arise between them.

/// Longest run of label characters kept in a socket file name. Labels are
/// free text, and a socket path has a hard length limit.
// limits-exempt: bounds a decorative file name fragment, not behavior.
const BRIDGE_NAME_LABEL_CHARS: usize = 24;

/// The label reduced to a file name fragment: ASCII letters, digits, `-` and `_`
/// only, at most `BRIDGE_NAME_LABEL_CHARS` of them. Other characters become
/// `_`, so a label can never inject a path separator.
fn bridge_name_fragment(label: &MachineLabel) -> String {
    label
        .as_str()
        .chars()
        .take(BRIDGE_NAME_LABEL_CHARS)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// A fresh socket path for one configured-machine attach bridge. The prefix is
/// distinct from the `shepr-ssh-` SSH config directories, whose sweep matches
/// on that prefix.
fn machine_bridge_path(runtime_dir: &std::path::Path, label: &MachineLabel) -> io::Result<PathBuf> {
    let (readable, short) = machine_bridge_names(label);
    shepr_platform::remote_bridge_endpoint_path(runtime_dir, &readable, short)
}

fn validate_machine_bridge_path(
    runtime_dir: &std::path::Path,
    label: &MachineLabel,
) -> io::Result<()> {
    let (readable, short) = machine_bridge_names(label);
    shepr_platform::validate_remote_bridge_endpoint_path(runtime_dir, &readable, short)
}

fn machine_bridge_names(label: &MachineLabel) -> (String, &'static str) {
    (
        format!("shepr-bridge-{}.sock", bridge_name_fragment(label)),
        "shepr-b.sock",
    )
}

fn is_launch_fatal_setup_error(error: &io::Error) -> bool {
    // Whether a local setup failure can never succeed on retry. Only the
    // bridge socket path and ssh config setup are classified here; discovery
    // errors never reach it. Invalid input, such as a runtime directory that can
    // never hold the bridge socket, is permanent.
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
    fn a_vanished_xdg_runtime_root_is_a_retryable_attention_failure() {
        // A successful rebuild needs a runtime root as short as a real
        // `/run/user/<uid>` for the SSH control socket name, which no scratch
        // directory is; this covers how the missing root is classified.
        let scratch = shepr_test_support::ScratchDir::new("machine-runtime-root-recovery");
        let xdg_runtime = scratch.join("xdg-runtime");
        let paths = shepr_config::AppPaths::rooted_at(&xdg_runtime, None, None);
        let target = SshTarget::parse("build.example").expect("test precondition");
        let mut state = ConnectorState::default();

        let error = ensure_managed_ssh_config(&mut state, &target, &paths)
            .expect_err("missing runtime root prevents SSH setup");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            !is_launch_fatal_setup_error(&error),
            "a runtime root that can return remains retryable: {error}"
        );
        assert!(
            !super::is_ssh_link_failure(&error),
            "a missing local runtime root is not a dropped SSH link"
        );
        assert!(
            super::super::SshFailureDiagnostic::from_error(&error).needs_attention(),
            "a missing local runtime root is actionable, not a dropped SSH link"
        );
        assert!(state.ssh.is_none(), "a failed setup keeps nothing to reuse");
    }

    fn cache_in(scratch: &shepr_test_support::ScratchDir) -> SshMetadataCache {
        let paths = shepr_config::AppPaths::rooted_at(scratch, None, None);
        SshMetadataCache::new(
            &paths,
            &SshTarget::parse("build.example").expect("test precondition"),
        )
    }

    fn executable(path: &str) -> RemoteExecutable {
        RemoteExecutable::parse(path).expect("test precondition")
    }

    #[test]
    fn only_remote_exec_failures_invalidate_a_remembered_path() {
        for exit_code in [126, 127] {
            let diagnostic = super::super::SshFailureDiagnostic::from_ssh_output(
                Some(exit_code),
                format!("remote command failed (exit status {exit_code})"),
            );
            let error = io::Error::other(diagnostic);
            assert!(remote_executable_must_be_rediscovered(&error));
        }

        // The text alone proves nothing: only ssh's typed report of the remote
        // command's exit status does.
        let quoted = io::Error::other("remote command failed (exit status 127)");
        assert!(!remote_executable_must_be_rediscovered(&quoted));
        let own_failure = io::Error::other(super::super::SshFailureDiagnostic::from_ssh_output(
            Some(super::super::SSH_OWN_FAILURE_EXIT_CODE),
            "ssh: connect to host h port 22: Connection refused".into(),
        ));
        assert!(!remote_executable_must_be_rediscovered(&own_failure));

        let server_mismatch = io::Error::new(
            io::ErrorKind::Unsupported,
            "SSH endpoint handshake failed: build mismatch: peer is a different shepr build",
        );
        assert!(!remote_executable_must_be_rediscovered(&server_mismatch));
        assert!(super::super::SshFailureDiagnostic::from_error(&server_mismatch).needs_attention());
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

        let error = resolve_remote_shepr(
            &cache,
            |_| Err(io::Error::new(io::ErrorKind::Unsupported, "another build")),
            || panic!("untyped probe failures must not trigger discovery"),
        )
        .expect_err("an untyped unsupported probe error is not proof of a stale executable");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(cache.load(), Some(executable("/new/shepr")));
    }

    #[test]
    fn a_link_failure_while_verifying_keeps_the_cache_and_skips_discovery() {
        let scratch = shepr_test_support::ScratchDir::new("machine-check-link");
        let cache = cache_in(&scratch);
        cache
            .store(&executable("/cached/shepr"))
            .expect("test precondition");
        let error = resolve_remote_shepr(
            &cache,
            |_| Err(io::Error::from(io::ErrorKind::TimedOut)),
            || panic!("a link failure says nothing about the cached path"),
        )
        .expect_err("link failure");
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
    fn bridge_paths_use_the_label_not_the_target() {
        let runtime_dir = shepr_test_support::ScratchDir::new("machine-bridge-paths");
        let label = |value: &str| MachineLabel::parse(value).expect("test precondition");
        let first =
            machine_bridge_path(runtime_dir.path(), &label("build")).expect("test precondition");
        let second =
            machine_bridge_path(runtime_dir.path(), &label("laptop")).expect("test precondition");
        assert_ne!(first, second);
        assert!(first.to_string_lossy().contains("shepr-bridge-build"));
        assert!(!first.to_string_lossy().contains("example.com"));
    }

    /// Labels are free text; only a bounded, path-safe fragment reaches the
    /// socket name.
    #[test]
    fn bridge_names_sanitize_and_bound_the_label() {
        let label = MachineLabel::parse("../etc/pass wd \u{e9}").expect("test precondition");
        assert_eq!(bridge_name_fragment(&label), "___etc_pass_wd__");
        let long = MachineLabel::parse("x".repeat(500)).expect("test precondition");
        assert_eq!(bridge_name_fragment(&long).len(), BRIDGE_NAME_LABEL_CHARS);
        let runtime_dir = shepr_test_support::ScratchDir::new("machine-bridge-sanitized");
        let path = machine_bridge_path(runtime_dir.path(), &label).expect("test precondition");
        assert_eq!(path.parent(), Some(runtime_dir.path()));
    }

    /// Two clients attached to one configured machine each bind a bridge socket of
    /// their own at the same time, and dropping them leaves the runtime
    /// directory empty.
    #[test]
    fn concurrent_bridges_for_one_machine_each_bind_their_own_socket() {
        let runtime_dir = shepr_test_support::ScratchDir::new("machine-bridge-concurrent");
        let label = MachineLabel::parse("build").expect("test precondition");
        let paths = [
            machine_bridge_path(runtime_dir.path(), &label),
            machine_bridge_path(runtime_dir.path(), &label),
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
        let compatibility =
            crate::remote_compatibility_error("matching Shepr is not ready; install or update");
        let compatibility = super::super::SshFailureDiagnostic::from_error(&compatibility);
        assert!(compatibility.needs_attention());
        assert!(compatibility.is_remote_compatibility());
        for kind in [
            io::ErrorKind::InvalidInput,
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
        ] {
            let error = crate::local_setup_error(
                "local setup failure",
                io::Error::new(kind, "the local operation failed"),
            );
            let diagnostic = super::super::SshFailureDiagnostic::from_error(&error);
            assert!(diagnostic.is_local_setup_failure(), "{kind}");
            assert!(diagnostic.needs_attention(), "{kind}");
        }
        for kind in [io::ErrorKind::InvalidData, io::ErrorKind::Unsupported] {
            let error = crate::local_setup_error(
                "local setup failure",
                io::Error::new(kind, "the local operation failed"),
            );
            let diagnostic = super::super::SshFailureDiagnostic::from_error(&error);
            assert!(diagnostic.is_local_setup_failure(), "{kind}");
            assert!(diagnostic.needs_attention(), "{kind}");
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
