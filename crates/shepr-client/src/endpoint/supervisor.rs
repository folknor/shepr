use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{ClientEndpointId, ClientEndpointStatus, NativeEndpointTransport};
pub(crate) use crate::limits::MAX_RETRY_DELAY;
use crate::limits::{
    ATTEMPT_BUDGET, ATTENTION_RETRY_DELAY, INITIAL_RETRY_DELAY, STABLE_CONNECTION_PERIOD,
};
use interprocess::TryClone as _;

// An attempt, and so the retry that follows it, must fit the retry bound.
const _: () = assert!(ATTEMPT_BUDGET.as_millis() < MAX_RETRY_DELAY.as_millis());

#[derive(Clone, Copy)]
pub(crate) struct EndpointConnectOptions {
    pub(crate) geometry: crate::handshake::HandshakeGeometry,
    pub(crate) mouse_capture: bool,
}

pub(crate) enum EndpointSupervisorEvent {
    Status {
        endpoint_id: ClientEndpointId,
        generation: u64,
        status: ClientEndpointStatus,
        message: shepr_remote::SshFailureDiagnostic,
        connector: Option<OwnedConnector>,
    },
    Connected {
        endpoint_id: ClientEndpointId,
        generation: u64,
        reader: shepr_platform::ipc::LocalStream,
        writer: NativeEndpointTransport,
        connector: Option<OwnedConnector>,
    },
}

impl EndpointSupervisorEvent {
    fn with_connector(self, connector: Option<OwnedConnector>) -> Self {
        match self {
            Self::Status {
                endpoint_id,
                generation,
                status,
                message,
                ..
            } => Self::Status {
                endpoint_id,
                generation,
                status,
                message,
                connector,
            },
            Self::Connected {
                endpoint_id,
                generation,
                reader,
                writer,
                ..
            } => Self::Connected {
                endpoint_id,
                generation,
                reader,
                writer,
                connector,
            },
        }
    }
}

/// A configured machine's connector, boxed so the events and targets that carry
/// it between the loop and an attempt stay small.
type OwnedConnector = Box<shepr_remote::MachineSshConnector>;

enum ConnectTarget {
    /// The Local server's client socket, and the guidance a build mismatch on
    /// it names: the plain `shepr` and `shepr server stop` commands, plus the
    /// socket overrides in effect. Resolved once from the client's paths, so the diagnostic every retry shows is the one the launch check
    /// would have printed.
    Local {
        path: PathBuf,
        mismatch_guidance: Arc<str>,
    },
    /// One connector per configured machine with the same target: it carries the
    /// launch-time ssh settings, the temporary ssh config and the remembered remote
    /// executable from one attempt to the next. An attempt takes ownership and returns it
    /// in its event.
    Ssh {
        connector: Option<OwnedConnector>,
        machine: shepr_config::MachineConfig,
    },
}

enum AttemptTarget {
    Local {
        path: PathBuf,
        mismatch_guidance: Arc<str>,
    },
    Ssh {
        connector: OwnedConnector,
    },
}

impl AttemptTarget {
    fn into_saved_connector(self) -> Option<OwnedConnector> {
        match self {
            Self::Local { .. } => None,
            Self::Ssh { connector, .. } => Some(connector),
        }
    }
}

struct ReconnectState {
    target: ConnectTarget,
    attempts: u32,
    next_attempt: Option<Instant>,
    in_flight: bool,
    /// When the attempt in flight started; its failure schedules the retry from here.
    attempt_started: Option<Instant>,
    generation: Option<shepr_protocol::ConnectionGeneration>,
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
    paths: shepr_config::AppPaths,
    next_generation: shepr_protocol::ConnectionGeneration,
    shutdown: Arc<AtomicBool>,
}

impl EndpointSupervisors {
    pub(crate) fn new(
        paths: &shepr_config::AppPaths,
        machines: &[shepr_config::MachineConfig],
        now: Instant,
    ) -> io::Result<Self> {
        let mut supervisors = Self {
            endpoints: HashMap::new(),
            paths: paths.clone(),
            next_generation: shepr_protocol::ConnectionGeneration::new(2),
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        for machine in machines {
            let connector = Box::new(shepr_remote::MachineSshConnector::new(
                paths,
                &machine.label,
                &machine.ssh,
            ));
            if let Some(error) = connector.launch_fatal_setup_error() {
                return Err(error);
            }
            supervisors.endpoints.insert(
                ClientEndpointId::Ssh(machine.label.clone()),
                ReconnectState::new(
                    ConnectTarget::Ssh {
                        connector: Some(connector),
                        machine: machine.clone(),
                    },
                    now,
                ),
            );
        }
        Ok(supervisors)
    }

    pub(crate) fn add_local(&mut self, path: PathBuf, generation: Option<u64>, now: Instant) {
        let mismatch_guidance =
            shepr_api::server_stop::restart_after_update_guidance_for(&self.paths).into();
        let mut state = ReconnectState::new(
            ConnectTarget::Local {
                path,
                mismatch_guidance,
            },
            now,
        );
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
            // Generations tell a live attempt's events from a stale one's, so
            // a generation is never reused. Running out is unreachable (one per
            // connection attempt); should it happen, the endpoint stops
            // retrying rather than issuing a duplicate.
            let Some(following_generation) = self.next_generation.checked_next() else {
                tracing::error!(
                    endpoint = ?endpoint_id,
                    "connection generations exhausted; not starting another attempt"
                );
                state.next_attempt = None;
                continue;
            };
            let target = match &mut state.target {
                ConnectTarget::Local {
                    path,
                    mismatch_guidance,
                } => AttemptTarget::Local {
                    path: path.clone(),
                    mismatch_guidance: Arc::clone(mismatch_guidance),
                },
                ConnectTarget::Ssh { connector, .. } => {
                    let Some(connector) = connector.take() else {
                        continue;
                    };
                    AttemptTarget::Ssh { connector }
                }
            };
            state.in_flight = true;
            state.attempt_started = Some(now);
            state.next_attempt = None;
            let generation = self.next_generation.get();
            state.generation = Some(generation.into());
            self.next_generation = following_generation;
            let endpoint_id = endpoint_id.clone();
            let event_tx = event_tx.clone();
            let shutdown = Arc::clone(&self.shutdown);
            let deadline = now + ATTEMPT_BUDGET;
            tokio::spawn(async move {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                let task_endpoint_id = endpoint_id.clone();
                // The attempt owns the saved connector and hands it back with
                // its event. A panic loses it with the task; the join error
                // below then reports no connector, and `return_connector`
                // rebuilds one from the machine so the endpoint still retries.
                let result = tokio::task::spawn_blocking(move || {
                    let mut target = target;
                    let result =
                        connect_once(&mut target, options, endpoint_id, generation, deadline);
                    (target.into_saved_connector(), result)
                })
                .await;
                let event = match result {
                    Ok((connector, Ok(event))) => event.with_connector(connector),
                    Ok((connector, Err(error))) => {
                        let failure = shepr_remote::SshFailureDiagnostic::from_error(&error);
                        EndpointSupervisorEvent::Status {
                            endpoint_id: task_endpoint_id,
                            generation,
                            status: if failure.needs_attention() {
                                ClientEndpointStatus::Attention
                            } else {
                                ClientEndpointStatus::Reconnecting
                            },
                            message: failure,
                            connector,
                        }
                    }
                    Err(error) => EndpointSupervisorEvent::Status {
                        endpoint_id: task_endpoint_id,
                        generation,
                        status: ClientEndpointStatus::Reconnecting,
                        message: shepr_remote::SshFailureDiagnostic::from_message(format!(
                            "endpoint connection task stopped unexpectedly: {error}"
                        )),
                        connector: None,
                    },
                };
                if !shutdown.load(Ordering::Acquire) {
                    // The send fails only once the client loop has exited and dropped its
                    // receiver; the returned event then drops here, releasing any
                    // connection it carries, which is all teardown needs.
                    event_tx.send(event).await.ok();
                }
            });
        }
    }

    /// Hands a finished attempt's connector back to its endpoint. An attempt
    /// that lost it (its blocking task died outside the panic guard) gets a
    /// fresh one built from the machine, so the endpoint never stalls without
    /// a connector. An attempt of an endpoint no longer supervised drops it.
    pub(crate) fn return_connector(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        connector: Option<OwnedConnector>,
    ) {
        let Some(state) = self.endpoints.get_mut(endpoint_id) else {
            return;
        };
        if state.generation != Some(generation.into()) {
            return;
        }
        let ConnectTarget::Ssh {
            connector: owned,
            machine,
        } = &mut state.target
        else {
            return;
        };
        if owned.is_some() {
            return;
        }
        *owned = Some(connector.unwrap_or_else(|| {
            tracing::warn!(
                endpoint = %endpoint_id.storage_key(),
                "a connection attempt lost its SSH connector; rebuilding it"
            );
            Box::new(shepr_remote::MachineSshConnector::new(
                &self.paths,
                &machine.label,
                &machine.ssh,
            ))
        }));
    }

    pub(crate) fn record_status(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        status: ClientEndpointStatus,
        now: Instant,
    ) -> bool {
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
    target: &mut AttemptTarget,
    options: EndpointConnectOptions,
    endpoint_id: ClientEndpointId,
    generation: u64,
    deadline: Instant,
) -> Result<EndpointSupervisorEvent, std::io::Error> {
    match target {
        AttemptTarget::Local {
            path,
            mismatch_guidance,
        } => {
            let stream =
                shepr_platform::ipc::connect_trusted_local_stream(path).map_err(|error| {
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
            establish(
                stream,
                EndpointLink::Local {
                    mismatch_guidance: mismatch_guidance.as_ref(),
                },
                options,
                endpoint_id,
                generation,
                deadline,
            )
        }
        AttemptTarget::Ssh { connector } => connector.connect(deadline, |connected| {
            establish(
                connected.stream,
                EndpointLink::Ssh(connected.bridge),
                options,
                endpoint_id.clone(),
                generation,
                deadline,
            )
        }),
    }
}

/// What carries one endpoint connection, for the handshake's diagnostics.
enum EndpointLink<'a> {
    Local { mismatch_guidance: &'a str },
    Ssh(shepr_remote::MachineSshBridge),
}

/// Handshakes over a fresh endpoint stream and hands the connection to the loop.
fn establish(
    mut stream: shepr_platform::ipc::LocalStream,
    link: EndpointLink<'_>,
    options: EndpointConnectOptions,
    endpoint_id: ClientEndpointId,
    generation: u64,
    deadline: Instant,
) -> Result<EndpointSupervisorEvent, std::io::Error> {
    let (ssh_bridge, mismatch_guidance) = match link {
        EndpointLink::Local { mismatch_guidance } => (None, Some(mismatch_guidance)),
        EndpointLink::Ssh(bridge) => (Some(bridge), None),
    };
    super::super::do_handshake(
        &mut stream,
        options.geometry,
        options.mouse_capture,
        false,
        Some(deadline),
    )
    .map_err(|error| {
        let error = handshake_error(error, mismatch_guidance);
        // An SSH endpoint that closes before Welcome usually means ssh itself failed
        // (network drop, auth, remote server launch). The bridge holds the real stderr;
        // prefer it so both the diagnostic and the attention classification see it.
        if error.kind() == std::io::ErrorKind::UnexpectedEof
            && let Some(failure) = ssh_bridge
                .as_ref()
                .and_then(shepr_remote::MachineSshBridge::reported_failure)
                .map(|failure| {
                    let kind = failure.kind();
                    let diagnostic = shepr_remote::SshFailureDiagnostic::from_error(&failure)
                        .with_context(HANDSHAKE_CONTEXT);
                    std::io::Error::new(kind, diagnostic)
                })
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
        connector: None,
    })
}

/// `mismatch_guidance` is the Local endpoint's way out of a build mismatch;
/// a configured machine has none here, its bridge reports its own.
fn handshake_error(error: crate::ClientError, mismatch_guidance: Option<&str>) -> std::io::Error {
    use crate::ClientError;
    use shepr_protocol::FramingError;
    let error = match error {
        ClientError::EndpointSetup(error)
        | ClientError::ConnectionFailed(error)
        | ClientError::ConnectionLost(error)
        | ClientError::HostTerminal(error)
        | ClientError::Protocol(FramingError::Io(error)) => error,
        ClientError::HandshakeRejected { error, .. } => {
            std::io::Error::new(std::io::ErrorKind::Unsupported, error)
        }
        ClientError::Preamble(shepr_protocol::preamble::PreambleError::DifferentBuild(peer))
            if mismatch_guidance.is_some() =>
        {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                local_build_mismatch(&peer.build_id, mismatch_guidance.unwrap_or_default()),
            )
        }
        ClientError::Preamble(
            error @ shepr_protocol::preamble::PreambleError::DifferentBuild(_),
        ) => std::io::Error::new(std::io::ErrorKind::Unsupported, error),
        ClientError::Preamble(error) => std::io::Error::new(std::io::ErrorKind::InvalidData, error),
        ClientError::UnexpectedWelcome => std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            crate::ClientError::UnexpectedWelcome,
        ),
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
    let diagnostic =
        shepr_remote::SshFailureDiagnostic::from_error(&error).with_context(HANDSHAKE_CONTEXT);
    std::io::Error::new(kind, diagnostic)
}

/// The shell status line and machine notice title supply the endpoint label;
/// keep only the failing phase here so it is not repeated in the displayed error.
const HANDSHAKE_CONTEXT: &str = "handshake failed";

/// The Local endpoint's build-mismatch diagnostic, on one line for the
/// endpoint status: both builds, then the guidance the launch
/// check prints when no configured machines keep the client running.
fn local_build_mismatch(running: &str, guidance: &str) -> String {
    format!(
        "build mismatch: the Local server runs shepr build {running}; this client is build {}. {}",
        shepr_protocol::BUILD_ID,
        guidance.replace('\n', " ")
    )
}

/// Endpoint reconnect backoff: doubling from `INITIAL_RETRY_DELAY` to the
/// `MAX_RETRY_DELAY` ceiling. This
/// policy is the client's alone; the other retry loops in the tree (the SSH
/// agent registration worker, the API accept loop, the CLI's status probe)
/// answer different failures and deliberately do not share it.
fn retry_delay(attempt: u32) -> Duration {
    INITIAL_RETRY_DELAY
        .saturating_mul(
            1_u32
                .checked_shl(attempt.saturating_sub(1))
                .unwrap_or(u32::MAX),
        )
        .min(MAX_RETRY_DELAY)
}

#[cfg(test)]
impl EndpointSupervisors {
    fn supervises(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints.contains_key(endpoint_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> shepr_config::MachineConfig {
        shepr_config::MachineConfig {
            label: shepr_config::MachineLabel::parse("Build").expect("test precondition"),
            ssh: shepr_config::SshTarget::parse("build").expect("test precondition"),
        }
    }

    /// Paths whose XDG runtime directory is as short as a real `/run/user/<uid>` and does
    /// not exist. These tests never connect, and no directory under the build tree is short
    /// enough for the SSH control socket's staging path. The missing directory fails the
    /// connector's runtime directory check with a plain `NotFound`, which is transient, not
    /// launch-fatal, and it is checked before any path length, so nothing is created or bound
    /// and the production launch check is untouched. Nothing reads the developer's config.
    fn short_runtime_paths() -> shepr_config::AppPaths {
        shepr_config::AppPaths::rooted_at(
            std::path::Path::new("/nonexistent/shepr-supervisor-tests"),
            None,
            None,
        )
    }

    /// `_env` is held by the caller only to keep the process environment isolated.
    fn supervisors_for(
        _env: &shepr_test_support::IsolatedEnv,
        machines: &[shepr_config::MachineConfig],
        now: Instant,
    ) -> EndpointSupervisors {
        EndpointSupervisors::new(&short_runtime_paths(), machines, now)
            .expect("test saved SSH setup is retryable")
    }

    #[test]
    fn saved_connector_moves_out_and_back_under_exclusive_ownership() {
        let env = shepr_test_support::IsolatedEnv::new();
        let now = Instant::now();
        let machine = machine();
        let id = ClientEndpointId::Ssh(machine.label.clone());
        let mut supervisors = supervisors_for(&env, &[machine], now);
        let state = supervisors
            .endpoints
            .get_mut(&id)
            .expect("test precondition");
        state.generation = Some(shepr_protocol::ConnectionGeneration::new(2));
        let ConnectTarget::Ssh { connector, .. } = &mut state.target else {
            panic!("a configured machine must have an SSH target");
        };
        let connector = connector.take().expect("test connector is present");
        assert!(matches!(
            &supervisors.endpoints[&id].target,
            ConnectTarget::Ssh {
                connector: None,
                ..
            }
        ));

        // A stale generation's connector is dropped, not installed.
        supervisors.return_connector(&id, 3, Some(connector));
        assert!(matches!(
            &supervisors.endpoints[&id].target,
            ConnectTarget::Ssh {
                connector: None,
                ..
            }
        ));

        // An attempt whose task died returns nothing; the endpoint gets a
        // rebuilt connector rather than stalling without one.
        supervisors.return_connector(&id, 2, None);

        assert!(matches!(
            &supervisors.endpoints[&id].target,
            ConnectTarget::Ssh {
                connector: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn brief_ssh_reconnections_do_not_reset_backoff() {
        let env = shepr_test_support::IsolatedEnv::new();
        let now = Instant::now();
        let machine = machine();
        let id = ClientEndpointId::Ssh(machine.label.clone());
        let mut supervisors = supervisors_for(&env, &[machine], now);
        supervisors
            .endpoints
            .get_mut(&id)
            .expect("test precondition")
            .generation = Some(shepr_protocol::ConnectionGeneration::new(2));
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
        // Open clients retry a machine within 30 seconds of it becoming reachable.
        let env = shepr_test_support::IsolatedEnv::new();
        let now = Instant::now();
        let machine = machine();
        let id = ClientEndpointId::Ssh(machine.label.clone());
        let mut supervisors = supervisors_for(&env, &[machine], now);
        supervisors
            .endpoints
            .get_mut(&id)
            .expect("test precondition")
            .generation = Some(shepr_protocol::ConnectionGeneration::new(2));
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
        let env = shepr_test_support::IsolatedEnv::new();
        let now = Instant::now();
        let machine = machine();
        let id = ClientEndpointId::Ssh(machine.label.clone());
        let mut supervisors = supervisors_for(&env, &[machine], now);
        supervisors
            .endpoints
            .get_mut(&id)
            .expect("test precondition")
            .generation = Some(shepr_protocol::ConnectionGeneration::new(9));

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
        // machine becomes reachable just after it started.
        assert!(ATTEMPT_BUDGET < MAX_RETRY_DELAY);
        let env = shepr_test_support::IsolatedEnv::new();
        for status in [
            ClientEndpointStatus::Reconnecting,
            ClientEndpointStatus::Attention,
        ] {
            let started = Instant::now();
            let machine = machine();
            let id = ClientEndpointId::Ssh(machine.label.clone());
            let mut supervisors = supervisors_for(&env, &[machine], started);
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
                state.generation = Some(shepr_protocol::ConnectionGeneration::new(7));
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
    fn launch_machines_are_supervised_and_due_immediately() {
        let env = shepr_test_support::IsolatedEnv::new();
        let now = Instant::now();
        let supervisors = supervisors_for(&env, &[machine()], now);
        let id = ClientEndpointId::Ssh(machine().label);
        assert!(supervisors.supervises(&id));
        assert_eq!(supervisors.endpoints[&id].next_attempt, Some(now));
        assert!(!supervisors_for(&env, &[], now).supervises(&id));
    }

    #[test]
    fn handshake_network_failures_retry_but_incompatibility_needs_attention() {
        let timeout = handshake_error(
            crate::ClientError::ConnectionLost(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out",
            )),
            None,
        );
        assert!(!shepr_remote::SshFailureDiagnostic::from_error(&timeout).needs_attention());
        let rejected = handshake_error(
            crate::ClientError::HandshakeRejected {
                error: shepr_protocol::HandshakeRefusal::InvalidSurface(
                    "surface capability missing".into(),
                ),
            },
            None,
        );
        assert_eq!(rejected.kind(), std::io::ErrorKind::Unsupported);
        assert!(shepr_remote::SshFailureDiagnostic::from_error(&rejected).needs_attention());
    }

    #[test]
    fn early_end_of_stream_and_shutdown_during_handshake_are_transient() {
        let eof = handshake_error(
            crate::ClientError::Protocol(shepr_protocol::FramingError::UnexpectedEof),
            None,
        );
        assert_eq!(eof.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(!shepr_remote::SshFailureDiagnostic::from_error(&eof).needs_attention());
        let shutdown = handshake_error(crate::ClientError::ServerShutdown { reason: None }, None);
        assert!(!shepr_remote::SshFailureDiagnostic::from_error(&shutdown).needs_attention());
        let malformed = handshake_error(
            crate::ClientError::Protocol(shepr_protocol::FramingError::Oversized {
                claimed: 2,
                max: 1,
            }),
            None,
        );
        assert!(shepr_remote::SshFailureDiagnostic::from_error(&malformed).needs_attention());
    }

    fn different_build() -> crate::ClientError {
        crate::ClientError::Preamble(shepr_protocol::preamble::PreambleError::DifferentBuild(
            shepr_protocol::preamble::PeerBuild {
                build_id: "00000000deadbeef".into(),
            },
        ))
    }

    /// With configured machines the launch check's refusal cannot fail the launch,
    /// so the Local endpoint's own diagnostic carries it: both builds, the
    /// forced stop and the plain attach command.
    #[test]
    fn a_local_build_mismatch_names_the_restart_guidance() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let paths = shepr_config::AppPaths::resolve().expect("isolated paths resolve");
        let now = Instant::now();
        let mut supervisors =
            EndpointSupervisors::new(&paths, &[], now).expect("test precondition");
        supervisors.add_local(paths.server_address().client_socket().into(), None, now);
        let ConnectTarget::Local {
            mismatch_guidance, ..
        } = &supervisors.endpoints[&ClientEndpointId::Local].target
        else {
            panic!("Local must have a Local target");
        };

        let error = handshake_error(different_build(), Some(&**mismatch_guidance));
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        let diagnostic = shepr_remote::SshFailureDiagnostic::from_error(&error);
        assert!(diagnostic.needs_attention());
        let message = diagnostic.to_string();
        for expected in [
            "handshake failed",
            "00000000deadbeef",
            shepr_protocol::BUILD_ID,
            "`shepr server stop`",
            "`shepr`",
        ] {
            assert!(message.contains(expected), "{expected}: {message}");
        }
        assert!(!message.contains('\n'), "{message}");
    }

    /// A configured machine's mismatch keeps the generic preamble text; its bridge
    /// and remote checks report the machine-specific way out.
    #[test]
    fn a_machine_build_mismatch_keeps_the_preamble_text() {
        let error = handshake_error(different_build(), None);
        assert!(error.to_string().contains("Install the same shepr build"));
        assert!(error.to_string().contains("handshake failed"));
    }

    #[test]
    fn local_in_attention_is_retried() {
        let env = shepr_test_support::IsolatedEnv::new();
        let now = Instant::now();
        let mut supervisors = supervisors_for(&env, &[], now);
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
        let env = shepr_test_support::IsolatedEnv::new();
        let now = Instant::now();
        let mut supervisors = supervisors_for(&env, &[machine()], now);
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
            supervisors.endpoints[&ClientEndpointId::Ssh(machine().label)].next_attempt,
            Some(now)
        );
    }

    #[test]
    fn ssh_recovery_rejects_stale_generations_and_rechecks_attention() {
        let env = shepr_test_support::IsolatedEnv::new();
        let now = Instant::now();
        let mut supervisors = supervisors_for(&env, &[machine()], now);
        let endpoint_id = ClientEndpointId::Ssh(machine().label);
        supervisors
            .endpoints
            .get_mut(&endpoint_id)
            .expect("test precondition")
            .generation = Some(shepr_protocol::ConnectionGeneration::new(4));
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
