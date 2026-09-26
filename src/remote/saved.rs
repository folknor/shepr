use std::io;
use std::path::PathBuf;

use super::attach::{
    DiscoveryProgress, RemoteShepr, RemoteSsh, SshStdioBridge,
    resume_installed_remote_shepr_discovery,
};

pub(crate) struct SavedSshBridge {
    bridge: SshStdioBridge,
}

impl SavedSshBridge {
    /// The SSH failure behind a connection that closed early, if the bridge reported one
    /// (waits briefly for the bridge thread). SSH stderr otherwise only reaches the log,
    /// and the caller would see a bare end of stream.
    pub(crate) fn reported_failure(&self) -> Option<io::Error> {
        self.bridge.reported_failure()
    }
}

pub(crate) struct SavedSshStream {
    pub(crate) stream: crate::ipc::LocalStream,
    pub(crate) bridge: SavedSshBridge,
}

/// Settings a saved-machine connector takes from the config. A client reads its
/// config once at launch and hands these in, so reconnects never re-read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SavedSshSettings {
    pub(crate) manage_ssh_config: bool,
}

/// Connects one saved SSH machine, repeatedly, for as long as a client keeps it enabled
/// with the same target and session (the client follows catalog edits and builds a new
/// connector when a machine is re-enabled or re-pointed).
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
pub(crate) struct SavedSshConnector {
    profile_id: String,
    target: String,
    session: String,
    settings: SavedSshSettings,
    state: std::sync::Mutex<ConnectorState>,
}

#[derive(Default)]
struct ConnectorState {
    ssh: Option<RemoteSsh>,
    remote_shepr: Option<RemoteShepr>,
    /// Full discovery's completed round trips, while it has not finished. Only kept while
    /// there is no remembered executable.
    discovery: DiscoveryProgress,
    seeded_from_disk: bool,
}

impl SavedSshConnector {
    pub(crate) fn new(
        profile_id: &str,
        target: &str,
        session: &str,
        settings: SavedSshSettings,
    ) -> Self {
        Self {
            profile_id: profile_id.to_owned(),
            target: target.to_owned(),
            session: session.to_owned(),
            settings,
            state: std::sync::Mutex::new(ConnectorState::default()),
        }
    }

    /// Starts a bridge and hands its stream to `establish`, which runs the endpoint
    /// handshake. The handshake is part of the attempt so that a failure there can
    /// still send the attempt back through discovery.
    ///
    /// Nothing SSH runs past `deadline`: discovery commands are cut short by it and no
    /// step starts once it has passed (the caller holds `establish` to it too). Without
    /// it, discovery and a remembered-executable retry could add up to minutes against
    /// a host that hangs, and the next attempt waits for this one.
    pub(crate) fn connect<T>(
        &self,
        deadline: std::time::Instant,
        mut establish: impl FnMut(SavedSshStream) -> io::Result<T>,
    ) -> io::Result<T> {
        validate_profile_path_id(&self.profile_id)?;
        crate::session::validate_name(&self.session)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let metadata_cache = crate::client::endpoint::SshMetadataCache::new(
            &self.profile_id,
            &self.target,
            &self.session,
        )?;
        // Attempts for one endpoint never overlap (the supervisor keeps one in flight, and
        // a replacement connector for the same profile waits for a retired one's attempt),
        // so holding the lock for the whole attempt contends with nothing.
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.seeded_from_disk {
            state.seeded_from_disk = true;
            state.remote_shepr = metadata_cache
                .load()
                .as_ref()
                .and_then(RemoteShepr::from_metadata);
        }
        if state
            .ssh
            .as_ref()
            .is_none_or(|ssh| ssh.missing_managed_config(self.settings.manage_ssh_config))
        {
            state.ssh = Some(RemoteSsh::new_noninteractive_with(
                self.target.clone(),
                self.settings.manage_ssh_config,
            ));
        }
        let ConnectorState {
            ssh,
            remote_shepr,
            discovery,
            ..
        } = &mut *state;
        let Some(ssh) = ssh.as_mut() else {
            return Err(io::Error::other("saved SSH transport is unavailable"));
        };
        ssh.set_attempt_deadline(Some(deadline));
        let ssh = &*ssh;

        if let Some(known) = remote_shepr.clone() {
            match self.attempt(ssh, &known, deadline, &mut establish) {
                Ok(connected) => return Ok(connected),
                Err(error) if super::attach::is_ssh_link_failure(&error) => return Err(error),
                Err(error) => {
                    tracing::debug!(
                        %error,
                        "remembered remote Shepr did not connect; rediscovering"
                    );
                    *remote_shepr = None;
                    metadata_cache.invalidate();
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
        match self.attempt(ssh, &discovered, deadline, &mut establish) {
            Ok(connected) => {
                if let Some(metadata) = discovered.machine_metadata() {
                    metadata_cache.store(&metadata);
                }
                Ok(connected)
            }
            Err(error) => {
                if !super::attach::is_ssh_link_failure(&error) {
                    *remote_shepr = None;
                }
                Err(error)
            }
        }
    }

    fn attempt<T>(
        &self,
        ssh: &RemoteSsh,
        remote_shepr: &RemoteShepr,
        deadline: std::time::Instant,
        establish: &mut impl FnMut(SavedSshStream) -> io::Result<T>,
    ) -> io::Result<T> {
        if std::time::Instant::now() >= deadline {
            return Err(super::attach::attempt_deadline_passed());
        }
        let path = saved_bridge_path(&self.profile_id);
        let bridge = SshStdioBridge::start(
            self.target.clone(),
            remote_shepr,
            path.clone(),
            &self.session,
            ssh.options(),
            true,
        )?;
        let stream = crate::ipc::connect_local_stream(&path)?;
        establish(SavedSshStream {
            stream,
            bridge: SavedSshBridge { bridge },
        })
    }
}

pub(crate) struct SavedSshApiBridge {
    path: PathBuf,
    bridge: SshStdioBridge,
    metadata_cache: crate::client::endpoint::SshMetadataCache,
    pub(crate) used_cached_metadata: bool,
}

impl SavedSshApiBridge {
    pub(crate) fn start(
        profile_id: &str,
        target: &str,
        session: &str,
        use_cached_metadata: bool,
    ) -> io::Result<Self> {
        let ssh = validated_saved_ssh(profile_id, target, session)?;
        let metadata_cache =
            crate::client::endpoint::SshMetadataCache::new(profile_id, target, session)?;
        let cached = use_cached_metadata.then(|| metadata_cache.load()).flatten();
        let used_cached_metadata = cached.is_some();
        let metadata = match cached {
            Some(metadata) => metadata,
            None => {
                let metadata = super::attach::discover_remote_api_metadata(&ssh, session)?;
                metadata_cache.store(&metadata);
                metadata
            }
        };
        let command = super::attach::cached_remote_api_command(&metadata, session);
        let path = crate::platform::remote_bridge_endpoint_path(
            &format!("shepr-api-ssh-{}-{profile_id}.sock", std::process::id()),
            &format!(
                "shepr-api-{}-{}.sock",
                std::process::id(),
                &profile_id[..16]
            ),
        );
        let bridge = SshStdioBridge::start_command(
            target.to_owned(),
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
        })
    }

    pub(crate) fn socket_path(&self) -> &std::path::Path {
        &self.path
    }

    pub(crate) fn reported_failure(&self) -> Option<io::Error> {
        self.bridge.reported_failure()
    }

    pub(crate) fn invalidate_metadata(&self) {
        self.metadata_cache.invalidate();
    }

    pub(crate) fn stale_metadata_failure(error: &io::Error) -> bool {
        error
            .to_string()
            .contains(super::attach::STALE_API_METADATA)
    }
}

pub(crate) fn saved_ssh_bootstrap_command(target: &str, session: &str) -> String {
    format!(
        "shepr --remote {} --session {}",
        super::shell_quote(target),
        super::shell_quote(session)
    )
}

pub(crate) fn saved_ssh_failure_needs_attention(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::InvalidInput
            | io::ErrorKind::InvalidData
            | io::ErrorKind::NotFound
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::Unsupported
    ) {
        return true;
    }
    let message = error.to_string().to_ascii_lowercase();
    [
        "permission denied",
        "host key verification failed",
        "remote host identification has changed",
        "no matching host key",
        "unsupported remote platform",
        "not ready",
        "install or update",
        "protocol",
        // Only a rejection is a compatibility problem; a bare "handshake" also matched
        // transient shutdowns that happened to occur mid-handshake.
        "handshake rejected",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn saved_bridge_path(profile_id: &str) -> PathBuf {
    let pid = std::process::id();
    let readable = format!("shepr-ssh-{pid}-{profile_id}.sock");
    let short = format!("shepr-s-{pid}-{}.sock", &profile_id[..16]);
    crate::platform::remote_bridge_endpoint_path(&readable, &short)
}

fn validated_saved_ssh(profile_id: &str, target: &str, session: &str) -> io::Result<RemoteSsh> {
    validate_profile_path_id(profile_id)?;
    crate::session::validate_name(session)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    Ok(RemoteSsh::new_noninteractive(target.to_owned()))
}

fn validate_profile_path_id(profile_id: &str) -> io::Result<()> {
    if profile_id.len() == 32
        && profile_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid SSH endpoint profile id",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_paths_use_profile_identity_not_target_or_session() {
        let first = saved_bridge_path("0123456789abcdef0123456789abcdef");
        let second = saved_bridge_path("fedcba9876543210fedcba9876543210");
        assert_ne!(first, second);
        assert!(!first.to_string_lossy().contains("example.com"));
        assert!(!first.to_string_lossy().contains("default"));
    }

    #[test]
    fn connector_rejects_invalid_profiles_before_touching_ssh() {
        let settings = SavedSshSettings {
            manage_ssh_config: false,
        };
        for (profile_id, session) in [
            ("not-a-profile-id", "agents"),
            ("0123456789abcdef0123456789abcdef", "bad session/name"),
        ] {
            let connector = SavedSshConnector::new(profile_id, "build", session, settings);
            let error = connector
                .connect(
                    std::time::Instant::now() + std::time::Duration::from_secs(30),
                    |_| -> io::Result<()> { panic!("no attempt may start") },
                )
                .expect_err("test precondition");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
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
        for message in [
            "Permission denied (publickey)",
            "Host key verification failed",
            "matching Shepr is not ready; install or update",
            "handshake rejected",
        ] {
            assert!(saved_ssh_failure_needs_attention(&io::Error::other(
                message
            )));
        }
        assert!(!saved_ssh_failure_needs_attention(&io::Error::new(
            io::ErrorKind::TimedOut,
            "network timed out"
        )));
        assert!(!saved_ssh_failure_needs_attention(&io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "server shut down during handshake"
        )));
    }
}
