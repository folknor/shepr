use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{ClientEndpointId, ClientEndpointStatus, NativeEndpointTransport};
use crate::protocol::ClientSurfaceSize;
use interprocess::TryClone as _;

const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);
/// Every endpoint, Local or saved machine, retries at least this often. `shepr machine
/// reconnect` tells the user that open clients retry within 30 seconds once the machine is
/// reachable again; a longer backoff for a reconnecting machine would make that untrue.
///
/// An attempt's own failure schedules the next one from when that attempt started, not
/// from when it gave up, and no attempt runs longer than `ATTEMPT_BUDGET`. Together they
/// keep the promise with an attempt already in flight: from any moment, the next attempt
/// starts once the current one ends or its retry delay (counted from its start) is up,
/// whichever is later, and both fall within 30 seconds.
pub(crate) const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const STABLE_CONNECTION_PERIOD: Duration = Duration::from_secs(60);
/// Same bound as `MAX_RETRY_DELAY`, for the same `shepr machine reconnect` promise.
const ATTENTION_RETRY_DELAY: Duration = Duration::from_secs(30);
/// The longest one connection attempt may run: the SSH discovery commands, the bridge and
/// the endpoint handshake all stop at this deadline. Without it an attempt against a host
/// that hangs ran for minutes (each discovery command may take 15 seconds, the handshake
/// 60), and the next attempt waited for it, which broke the 30-second reconnect promise.
///
/// A healthy attempt needs far less: every noninteractive discovery command already had
/// to fit a cold SSH connect into 15 seconds. It stays below `MAX_RETRY_DELAY` to leave
/// room for tearing a timed-out bridge down.
///
/// The budget is the same for every attempt, including one that has to run full
/// discovery of the remote executable. Most attempts do not: `shepr machine add` seeds
/// the metadata cache and a reconnect launches the bridge from the remembered executable.
/// With the default managed ssh config every command after the first reuses one shared
/// connection (ControlMaster, persisting ten minutes), so only one cold connect is paid.
/// The case that can overrun is a cache miss or a stale remembered path on a slow link
/// without connection sharing, where each of discovery's round trips (up to three
/// commands, a status probe per candidate, then the bridge) is its own cold connect.
/// That case is handled by resuming, not by a larger budget: the saved-machine connector
/// keeps what discovery completed when an attempt ends on a timeout or other link
/// failure (any other error clears it) and the next attempt continues from there, and it
/// keeps a freshly discovered executable when only the bridge ran out of time. No
/// discovery round trip may take longer than 15 seconds, so every attempt that starts
/// with discovery completes at least one, and discovery finishes after a bounded number
/// of attempts; after that the bridge and handshake need to fit one attempt, as on every
/// ordinary reconnect. A larger budget for discovery
/// attempts would stretch the 30-second reconnect promise exactly where the link is
/// slowest, and would still fail on a link one step slower.
const ATTEMPT_BUDGET: Duration = Duration::from_secs(25);

#[derive(Clone, Copy)]
pub(crate) struct EndpointConnectOptions {
    pub(crate) geometry: crate::geometry::HostGeometry,
    pub(crate) surface_size: ClientSurfaceSize,
    pub(crate) mouse_capture: bool,
}

pub(crate) enum EndpointSupervisorEvent {
    Status {
        endpoint_id: ClientEndpointId,
        generation: u64,
        status: ClientEndpointStatus,
        message: crate::remote::SshFailureDiagnostic,
    },
    Connected {
        endpoint_id: ClientEndpointId,
        generation: u64,
        reader: crate::ipc::LocalStream,
        writer: NativeEndpointTransport,
    },
}

#[derive(Clone)]
enum ConnectTarget {
    Local(PathBuf),
    /// One connector per saved machine with the same target and session: it carries the
    /// launch-time ssh settings, the temporary ssh config and the remembered remote
    /// executable from one attempt to the next. A catalog change that retires the machine
    /// drops it; adding it again builds a new one.
    Ssh(Arc<crate::remote::SavedSshConnector>),
}

struct ReconnectState {
    target: ConnectTarget,
    attempts: u32,
    next_attempt: Option<Instant>,
    in_flight: bool,
    /// When the attempt in flight started; its failure schedules the retry from here.
    attempt_started: Option<Instant>,
    generation: Option<crate::protocol::ConnectionGeneration>,
    online_since: Option<Instant>,
}

impl ReconnectState {
    fn new(target: ConnectTarget, now: Instant) -> Self {
        Self {
            target,
            attempts: 0,
            next_attempt: Some(now),
            in_flight: false,
            attempt_started: None,
            generation: None,
            online_since: None,
        }
    }
}

pub(crate) struct EndpointSupervisors {
    endpoints: HashMap<ClientEndpointId, ReconnectState>,
    /// Launch-time ssh settings (config is read once), applied to every saved machine,
    /// including ones added to the catalog while the client runs.
    ssh_settings: crate::remote::SavedSshSettings,
    paths: crate::config::AppPaths,
    /// Attempts still running for endpoints that were retired mid-attempt, by generation.
    /// A saved machine's bridge socket path is derived from its profile id, so a restarted
    /// supervisor for the same id must not start its own attempt until this one reports.
    retired_attempts: HashMap<ClientEndpointId, crate::protocol::ConnectionGeneration>,
    next_generation: crate::protocol::ConnectionGeneration,
    shutdown: Arc<AtomicBool>,
}

impl EndpointSupervisors {
    pub(crate) fn with_ssh_settings(
        paths: &crate::config::AppPaths,
        profiles: &[super::SavedSshEndpoint],
        settings: crate::remote::SavedSshSettings,
        now: Instant,
    ) -> Self {
        let mut supervisors = Self {
            endpoints: HashMap::new(),
            ssh_settings: settings,
            paths: paths.clone(),
            retired_attempts: HashMap::new(),
            next_generation: crate::protocol::ConnectionGeneration::new(2),
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        for profile in profiles {
            supervisors.start_ssh(profile, now);
        }
        supervisors
    }

    /// Supervises a saved machine with a fresh connector, replacing any previous one for
    /// the same profile id. The first attempt is due immediately, unless a retired attempt
    /// for the same id is still running; then it is due as soon as that one reports.
    pub(crate) fn start_ssh(&mut self, profile: &super::SavedSshEndpoint, now: Instant) {
        let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
        self.retire(&endpoint_id);
        let connector = crate::remote::SavedSshConnector::new(
            &self.paths,
            &profile.id,
            &profile.target,
            &profile.session,
            self.ssh_settings,
        );
        let mut state = ReconnectState::new(ConnectTarget::Ssh(Arc::new(connector)), now);
        if self.retired_attempts.contains_key(&endpoint_id) {
            state.next_attempt = None;
        }
        self.endpoints.insert(endpoint_id, state);
    }

    /// Stops supervising an endpoint and drops its connector. An attempt already in flight
    /// still finishes, but its event carries a generation nothing records any more, so
    /// `record_status` rejects it and the client loop drops the connection it delivers.
    pub(crate) fn retire(&mut self, endpoint_id: &ClientEndpointId) {
        if let Some(state) = self.endpoints.remove(endpoint_id)
            && state.in_flight
            && let Some(generation) = state.generation
        {
            self.retired_attempts
                .insert(endpoint_id.clone(), generation);
        }
    }

    pub(crate) fn supervises(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints.contains_key(endpoint_id)
    }

    pub(crate) fn add_local(&mut self, path: PathBuf, generation: Option<u64>, now: Instant) {
        let mut state = ReconnectState::new(ConnectTarget::Local(path), now);
        state.generation = generation.map(Into::into);
        if generation.is_some() {
            state.next_attempt = None;
        }
        self.endpoints.insert(ClientEndpointId::Local, state);
    }

    pub(crate) fn spawn_due(
        &mut self,
        now: Instant,
        options: EndpointConnectOptions,
        event_tx: &tokio::sync::mpsc::Sender<EndpointSupervisorEvent>,
    ) {
        for (endpoint_id, state) in &mut self.endpoints {
            if state.in_flight || state.next_attempt.is_none_or(|deadline| deadline > now) {
                continue;
            }
            state.in_flight = true;
            state.attempt_started = Some(now);
            state.next_attempt = None;
            let generation = self.next_generation.get();
            state.generation = Some(generation.into());
            self.next_generation = self.next_generation.next();
            let endpoint_id = endpoint_id.clone();
            let target = state.target.clone();
            let event_tx = event_tx.clone();
            let shutdown = Arc::clone(&self.shutdown);
            let deadline = now + ATTEMPT_BUDGET;
            tokio::spawn(async move {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                let task_endpoint_id = endpoint_id.clone();
                let result = tokio::task::spawn_blocking(move || {
                    connect_once(&target, options, endpoint_id, generation, deadline)
                })
                .await;
                let event = match result {
                    Ok(Ok(event)) => event,
                    Ok(Err(error)) => {
                        let failure = crate::remote::SshFailureDiagnostic::from_error(&error);
                        EndpointSupervisorEvent::Status {
                            endpoint_id: task_endpoint_id,
                            generation,
                            status: if failure.needs_attention() {
                                ClientEndpointStatus::Attention
                            } else {
                                ClientEndpointStatus::Reconnecting
                            },
                            message: failure,
                        }
                    }
                    Err(error) => EndpointSupervisorEvent::Status {
                        endpoint_id: task_endpoint_id,
                        generation,
                        status: ClientEndpointStatus::Reconnecting,
                        message: crate::remote::SshFailureDiagnostic::from_message(format!(
                            "endpoint connection task stopped unexpectedly: {error}"
                        )),
                    },
                };
                if !shutdown.load(Ordering::Acquire) {
                    let _ = event_tx.send(event).await;
                }
            });
        }
    }

    pub(crate) fn record_status(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        status: ClientEndpointStatus,
        now: Instant,
    ) -> bool {
        if self.retired_attempts.get(endpoint_id) == Some(&generation.into()) {
            // The retired attempt has finished, so its bridge socket is free again. Its
            // outcome belongs to the retired connector and is not recorded.
            self.retired_attempts.remove(endpoint_id);
            if let Some(state) = self
                .endpoints
                .get_mut(endpoint_id)
                .filter(|state| !state.in_flight && state.generation.is_none())
            {
                state.next_attempt = Some(now);
            }
            return false;
        }
        let Some(state) = self.endpoints.get_mut(endpoint_id) else {
            return false;
        };
        if state.generation != Some(generation.into()) {
            return false;
        }
        // An attempt's own outcome counts its retry delay from when it started, so time
        // spent inside a slow attempt is not waited out a second time. A connection that
        // was established and later dropped counts from now.
        let attempt_started = state.attempt_started.take();
        let retry_base = if state.in_flight {
            attempt_started.map_or(now, |started| started.min(now))
        } else {
            now
        };
        state.in_flight = false;
        match status {
            ClientEndpointStatus::Online => {
                if endpoint_id.is_local() {
                    state.attempts = 0;
                }
                state.online_since.get_or_insert(now);
                state.next_attempt = None;
            }
            ClientEndpointStatus::Attention => {
                state.online_since = None;
                // Authentication, configuration or a server version may be repaired outside
                // this client, and the client UI has no manual reconnect. Local is included:
                // restarting or upgrading its server is exactly such a repair, and with no
                // retry a Local in attention stayed dead until the client restarted.
                state.next_attempt = Some(retry_base + ATTENTION_RETRY_DELAY);
            }
            ClientEndpointStatus::Connecting | ClientEndpointStatus::Reconnecting => {
                // A brief maintenance wake can complete a handshake without restoring the link.
                if state.online_since.take().is_some_and(|connected| {
                    now.saturating_duration_since(connected) >= STABLE_CONNECTION_PERIOD
                }) {
                    state.attempts = 0;
                }
                state.attempts = state.attempts.saturating_add(1);
                state.next_attempt = Some(retry_base + retry_delay(state.attempts));
            }
        }
        true
    }

    pub(crate) fn disconnected(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        now: Instant,
    ) -> bool {
        self.record_status(
            endpoint_id,
            generation,
            ClientEndpointStatus::Reconnecting,
            now,
        )
    }
}

impl Drop for EndpointSupervisors {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

fn connect_once(
    target: &ConnectTarget,
    options: EndpointConnectOptions,
    endpoint_id: ClientEndpointId,
    generation: u64,
    deadline: Instant,
) -> Result<EndpointSupervisorEvent, std::io::Error> {
    match target {
        ConnectTarget::Local(path) => {
            let stream = crate::ipc::connect_local_stream(path).map_err(|error| {
                // An absent Local socket is transient, unlike a missing SSH install.
                if error.kind() == std::io::ErrorKind::NotFound {
                    std::io::Error::new(
                        std::io::ErrorKind::ConnectionRefused,
                        "Local is unavailable; start its server to reconnect",
                    )
                } else {
                    error
                }
            })?;
            establish(stream, None, options, endpoint_id, generation, deadline)
        }
        ConnectTarget::Ssh(connector) => connector.connect(deadline, |connected| {
            establish(
                connected.stream,
                Some(connected.bridge),
                options,
                endpoint_id.clone(),
                generation,
                deadline,
            )
        }),
    }
}

/// Handshakes over a fresh endpoint stream and hands the connection to the loop.
fn establish(
    mut stream: crate::ipc::LocalStream,
    ssh_bridge: Option<crate::remote::SavedSshBridge>,
    options: EndpointConnectOptions,
    endpoint_id: ClientEndpointId,
    generation: u64,
    deadline: Instant,
) -> Result<EndpointSupervisorEvent, std::io::Error> {
    super::super::do_handshake(
        &mut stream,
        crate::client::handshake::ClientProcessRole::Local,
        options.geometry,
        Some(options.surface_size),
        options.mouse_capture,
        false,
        Some(deadline),
    )
    .map_err(|error| {
        let error = handshake_error(error);
        // An SSH endpoint that closes before Welcome usually means ssh itself failed
        // (network drop, auth, remote server launch). The bridge holds the real stderr;
        // prefer it so both the diagnostic and the attention classification see it.
        if error.kind() == std::io::ErrorKind::UnexpectedEof
            && let Some(failure) = ssh_bridge
                .as_ref()
                .and_then(crate::remote::SavedSshBridge::reported_failure)
        {
            failure
        } else {
            error
        }
    })?;
    let lifetime: Box<dyn Send> = match ssh_bridge {
        Some(bridge) => Box::new(bridge),
        None => Box::new(()),
    };
    // No encoding or capability checks: the handshake's build-identity
    // preamble already proved the endpoint is this same build, so it speaks
    // the semantic client shell and has every capability this build has.
    let reader = stream.try_clone()?;
    let writer = NativeEndpointTransport::with_lifetime(stream, lifetime)?;
    Ok(EndpointSupervisorEvent::Connected {
        endpoint_id,
        generation,
        reader,
        writer,
    })
}

fn handshake_error(error: crate::client::ClientError) -> std::io::Error {
    use crate::client::ClientError;
    use crate::protocol::FramingError;
    let error = match error {
        ClientError::ConnectionFailed(error) | ClientError::ConnectionLost(error) => error,
        ClientError::HostTerminal(error) => error,
        ClientError::HandshakeRejected { error, .. } => {
            std::io::Error::new(std::io::ErrorKind::Unsupported, error)
        }
        ClientError::Preamble(
            error @ crate::protocol::preamble::PreambleError::DifferentBuild(_),
        ) => std::io::Error::new(std::io::ErrorKind::Unsupported, error),
        ClientError::Preamble(error) => std::io::Error::new(std::io::ErrorKind::InvalidData, error),
        ClientError::UnexpectedWelcome { endpoint } => std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            crate::client::ClientError::UnexpectedWelcome { endpoint },
        ),
        ClientError::SurfaceUpdateBeforeDecode => std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            crate::client::ClientError::SurfaceUpdateBeforeDecode,
        ),
        ClientError::Protocol(FramingError::Io(error)) => error,
        // A peer that closes before Welcome is a server restarting, a dropped SSH link or a
        // remote launch that failed: all transient, so this must stay out of InvalidData,
        // which the attention classifier treats as a compatibility problem.
        ClientError::Protocol(FramingError::UnexpectedEof) => std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "connection closed before the endpoint finished connecting",
        ),
        ClientError::Protocol(error) => {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        }
        ClientError::ServerShutdown { reason } => std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            reason.map_or_else(
                || "server shut down while connecting".into(),
                |reason| reason.to_string(),
            ),
        ),
    };
    let kind = error.kind();
    std::io::Error::new(
        kind,
        crate::remote::SshFailureDiagnostic::from_error(&error),
    )
}

fn retry_delay(attempt: u32) -> Duration {
    INITIAL_RETRY_DELAY
        .saturating_mul(
            1_u32
                .checked_shl(attempt.saturating_sub(1).min(8))
                .unwrap_or(u32::MAX),
        )
        .min(MAX_RETRY_DELAY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::endpoint::ProfileId;

    fn profile() -> super::super::SavedSshEndpoint {
        super::super::SavedSshEndpoint {
            id: ProfileId::parse("0123456789abcdef0123456789abcdef").expect("test precondition"),
            label: "Build".into(),
            target: crate::remote::SshTarget::parse("build").expect("test precondition"),
            session: "agents".into(),
        }
    }

    /// Tests never read the developer's own config file.
    fn supervisors_for(
        profiles: &[super::super::SavedSshEndpoint],
        now: Instant,
    ) -> EndpointSupervisors {
        EndpointSupervisors::with_ssh_settings(
            &crate::config::AppPaths::default(),
            profiles,
            crate::remote::SavedSshSettings {
                manage_ssh_config: false,
            },
            now,
        )
    }

    #[test]
    fn every_attempt_for_an_endpoint_reuses_one_connector() {
        let now = Instant::now();
        let profile = profile();
        let id = ClientEndpointId::Ssh(profile.id.clone());
        let supervisors = supervisors_for(&[profile], now);
        let ConnectTarget::Ssh(connector) = &supervisors.endpoints[&id].target else {
            panic!("saved machine must have an SSH target");
        };
        // `spawn_due` clones the target per attempt; the connector (and so the settings,
        // the ssh config and the remembered executable) is shared, never rebuilt.
        let ConnectTarget::Ssh(attempt) = supervisors.endpoints[&id].target.clone() else {
            panic!("saved machine must have an SSH target");
        };
        assert!(Arc::ptr_eq(connector, &attempt));
    }

    #[test]
    fn brief_ssh_reconnections_do_not_reset_backoff() {
        let now = Instant::now();
        let profile = profile();
        let id = ClientEndpointId::Ssh(profile.id.clone());
        let mut supervisors = supervisors_for(&[profile], now);
        supervisors
            .endpoints
            .get_mut(&id)
            .expect("test precondition")
            .generation = Some(crate::protocol::ConnectionGeneration::new(2));
        for attempt in 1..=5 {
            let connected = now + Duration::from_secs(attempt * 20);
            assert!(supervisors.record_status(&id, 2, ClientEndpointStatus::Online, connected));
            let failed = connected + Duration::from_secs(15);
            assert!(supervisors.disconnected(&id, 2, failed));
            assert_eq!(
                supervisors.endpoints[&id].next_attempt,
                Some(failed + INITIAL_RETRY_DELAY * (1 << (attempt - 1)))
            );
        }
        let connected = now + Duration::from_secs(200);
        assert!(supervisors.record_status(&id, 2, ClientEndpointStatus::Online, connected));
        let failed = connected + Duration::from_secs(60);
        assert!(supervisors.disconnected(&id, 2, failed));
        assert_eq!(
            supervisors.endpoints[&id].next_attempt,
            Some(failed + INITIAL_RETRY_DELAY)
        );
    }

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(retry_delay(1), INITIAL_RETRY_DELAY);
        assert_eq!(retry_delay(100), MAX_RETRY_DELAY);
    }

    #[test]
    fn a_reconnecting_machine_retries_within_thirty_seconds() {
        // `shepr machine reconnect` promises open clients retry within 30 seconds.
        let now = Instant::now();
        let profile = profile();
        let id = ClientEndpointId::Ssh(profile.id.clone());
        let mut supervisors = supervisors_for(&[profile], now);
        supervisors
            .endpoints
            .get_mut(&id)
            .expect("test precondition")
            .generation = Some(crate::protocol::ConnectionGeneration::new(2));
        for _ in 0..20 {
            assert!(supervisors.disconnected(&id, 2, now));
            assert!(
                supervisors.endpoints[&id]
                    .next_attempt
                    .is_some_and(|next| next <= now + Duration::from_secs(30))
            );
        }
    }

    #[test]
    fn unavailable_saved_machine_stays_supervised_and_retries() {
        let now = Instant::now();
        let profile = profile();
        let id = ClientEndpointId::Ssh(profile.id.clone());
        let mut supervisors = supervisors_for(&[profile], now);
        supervisors
            .endpoints
            .get_mut(&id)
            .expect("test precondition")
            .generation = Some(crate::protocol::ConnectionGeneration::new(9));

        assert!(supervisors.record_status(&id, 9, ClientEndpointStatus::Attention, now));
        assert_eq!(
            supervisors.endpoints[&id].next_attempt,
            Some(now + ATTENTION_RETRY_DELAY)
        );
        assert!(supervisors.supervises(&id));

        let retry_at = now + ATTENTION_RETRY_DELAY;
        assert!(supervisors.record_status(&id, 9, ClientEndpointStatus::Reconnecting, retry_at,));
        assert_eq!(
            supervisors.endpoints[&id].next_attempt,
            Some(retry_at + INITIAL_RETRY_DELAY)
        );
        assert!(supervisors.supervises(&id));
    }

    #[test]
    fn a_slow_failed_attempt_still_retries_within_thirty_seconds_of_any_moment() {
        // An attempt that hangs until its budget runs out, at the longest backoff, and the
        // user runs `shepr machine reconnect` just after it started.
        assert!(ATTEMPT_BUDGET < MAX_RETRY_DELAY);
        for status in [
            ClientEndpointStatus::Reconnecting,
            ClientEndpointStatus::Attention,
        ] {
            let started = Instant::now();
            let profile = profile();
            let id = ClientEndpointId::Ssh(profile.id.clone());
            let mut supervisors = supervisors_for(&[profile], started);
            {
                // What `spawn_due` does when it launches an attempt.
                let state = supervisors
                    .endpoints
                    .get_mut(&id)
                    .expect("test precondition");
                state.attempts = 30;
                state.in_flight = true;
                state.attempt_started = Some(started);
                state.next_attempt = None;
                state.generation = Some(crate::protocol::ConnectionGeneration::new(7));
            }
            let promised_at = started + Duration::from_millis(10);
            let gave_up = started + ATTEMPT_BUDGET;
            assert!(supervisors.record_status(&id, 7, status, gave_up));
            let next = supervisors.endpoints[&id]
                .next_attempt
                .expect("a failed attempt is retried");
            assert!(
                next <= promised_at + MAX_RETRY_DELAY,
                "{status:?}: retry {:?} after the promise",
                next.saturating_duration_since(promised_at)
            );
            // The retry delay counts from the attempt's start, not from when it gave up.
            assert!(next < gave_up + MAX_RETRY_DELAY);
            assert!(supervisors.endpoints[&id].attempt_started.is_none());
        }
    }

    #[test]
    fn catalog_changes_start_and_retire_machines() {
        let now = Instant::now();
        let mut supervisors = supervisors_for(&[], now);
        let profile = profile();
        let id = ClientEndpointId::Ssh(profile.id.clone());
        assert!(!supervisors.supervises(&id));

        supervisors.start_ssh(&profile, now);
        assert!(supervisors.supervises(&id));
        assert_eq!(supervisors.endpoints[&id].next_attempt, Some(now));

        supervisors.retire(&id);
        assert!(!supervisors.supervises(&id));
        // Nothing records a retired machine's events any more.
        assert!(!supervisors.record_status(&id, 2, ClientEndpointStatus::Online, now));
    }

    #[test]
    fn a_restarted_machine_waits_for_its_retired_attempt_to_report() {
        let now = Instant::now();
        let profile = profile();
        let id = ClientEndpointId::Ssh(profile.id.clone());
        let mut supervisors = supervisors_for(std::slice::from_ref(&profile), now);
        {
            // What `spawn_due` does when it launches an attempt.
            let state = supervisors
                .endpoints
                .get_mut(&id)
                .expect("test precondition");
            state.in_flight = true;
            state.next_attempt = None;
            state.generation = Some(crate::protocol::ConnectionGeneration::new(5));
        }

        // Removed and re-added (or re-pointed) while that attempt still runs: the new
        // connector shares the profile's bridge socket path, so it must not start yet.
        supervisors.retire(&id);
        supervisors.start_ssh(&profile, now);
        assert_eq!(supervisors.endpoints[&id].next_attempt, None);

        // The retired attempt's outcome is not recorded, but frees the path.
        let later = now + Duration::from_secs(3);
        assert!(!supervisors.record_status(&id, 5, ClientEndpointStatus::Online, later));
        assert_eq!(supervisors.endpoints[&id].next_attempt, Some(later));
        assert!(supervisors.retired_attempts.is_empty());
    }

    #[test]
    fn handshake_network_failures_retry_but_incompatibility_needs_attention() {
        let timeout = handshake_error(crate::client::ClientError::ConnectionLost(
            std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"),
        ));
        assert!(!crate::remote::SshFailureDiagnostic::from_error(&timeout).needs_attention());
        let rejected = handshake_error(crate::client::ClientError::HandshakeRejected {
            error: crate::protocol::HandshakeRefusal::InvalidSurface(
                "surface capability missing".into(),
            ),
        });
        assert_eq!(rejected.kind(), std::io::ErrorKind::Unsupported);
        assert!(crate::remote::SshFailureDiagnostic::from_error(&rejected).needs_attention());
    }

    #[test]
    fn early_end_of_stream_and_shutdown_during_handshake_are_transient() {
        let eof = handshake_error(crate::client::ClientError::Protocol(
            crate::protocol::FramingError::UnexpectedEof,
        ));
        assert_eq!(eof.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(!crate::remote::SshFailureDiagnostic::from_error(&eof).needs_attention());
        let shutdown = handshake_error(crate::client::ClientError::ServerShutdown { reason: None });
        assert!(!crate::remote::SshFailureDiagnostic::from_error(&shutdown).needs_attention());
        let malformed = handshake_error(crate::client::ClientError::Protocol(
            crate::protocol::FramingError::Oversized { claimed: 2, max: 1 },
        ));
        assert!(crate::remote::SshFailureDiagnostic::from_error(&malformed).needs_attention());
    }

    #[test]
    fn local_in_attention_is_retried() {
        let now = Instant::now();
        let mut supervisors = supervisors_for(&[], now);
        supervisors.add_local(PathBuf::from("local.sock"), Some(1), now);
        assert!(supervisors.record_status(
            &ClientEndpointId::Local,
            1,
            ClientEndpointStatus::Attention,
            now
        ));
        assert_eq!(
            supervisors.endpoints[&ClientEndpointId::Local].next_attempt,
            Some(now + ATTENTION_RETRY_DELAY)
        );
    }

    #[test]
    fn healthy_local_only_retries_after_its_connection_fails() {
        let now = Instant::now();
        let mut supervisors = supervisors_for(&[profile()], now);
        supervisors.add_local(PathBuf::from("local.sock"), Some(1), now);
        assert!(
            supervisors.endpoints[&ClientEndpointId::Local]
                .next_attempt
                .is_none()
        );
        assert!(!supervisors.disconnected(&ClientEndpointId::Local, 0, now));
        assert!(
            supervisors.endpoints[&ClientEndpointId::Local]
                .next_attempt
                .is_none()
        );
        assert!(supervisors.disconnected(&ClientEndpointId::Local, 1, now));
        assert_eq!(
            supervisors.endpoints[&ClientEndpointId::Local].next_attempt,
            Some(now + INITIAL_RETRY_DELAY)
        );
        assert_eq!(
            supervisors.endpoints[&ClientEndpointId::Ssh(profile().id)].next_attempt,
            Some(now)
        );
    }

    #[test]
    fn ssh_recovery_rejects_stale_generations_and_rechecks_attention() {
        let now = Instant::now();
        let mut supervisors = supervisors_for(&[profile()], now);
        let endpoint_id = ClientEndpointId::Ssh(profile().id);
        supervisors
            .endpoints
            .get_mut(&endpoint_id)
            .expect("test precondition")
            .generation = Some(crate::protocol::ConnectionGeneration::new(4));
        assert!(supervisors.record_status(&endpoint_id, 4, ClientEndpointStatus::Online, now));
        assert!(!supervisors.disconnected(&endpoint_id, 3, now));
        assert!(supervisors.endpoints[&endpoint_id].next_attempt.is_none());
        assert!(supervisors.disconnected(&endpoint_id, 4, now));
        assert_eq!(
            supervisors.endpoints[&endpoint_id].next_attempt,
            Some(now + INITIAL_RETRY_DELAY)
        );
        assert!(supervisors.record_status(&endpoint_id, 4, ClientEndpointStatus::Attention, now));
        assert_eq!(
            supervisors.endpoints[&endpoint_id].next_attempt,
            Some(now + Duration::from_secs(30))
        );
    }
}
