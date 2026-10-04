use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{ClientEndpointId, ClientEndpointStatus, EndpointFailureStatus};
use crate::events::ClientLoopEvent;
use crate::limits::{
    ATTEMPT_BUDGET, ATTENTION_RETRY_DELAY, INITIAL_RETRY_DELAY, MAX_RETRY_DELAY,
    STABLE_CONNECTION_PERIOD,
};

#[derive(Clone, Copy)]
pub(crate) struct EndpointConnectOptions {
    pub(crate) geometry: shepr_protocol::TerminalGeometry,
    pub(crate) mouse_capture: bool,
}

pub(crate) enum EndpointSupervisorEvent {
    Status {
        endpoint_id: ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
        status: EndpointFailureStatus,
        message: shepr_launch::EndpointFailure,
        connector: Option<OwnedConnector>,
    },
    Connected {
        endpoint_id: ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
        connection: crate::endpoint::connection_io::EndpointConnectionIo,
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
                connection,
                ..
            } => Self::Connected {
                endpoint_id,
                generation,
                connection,
                connector,
            },
        }
    }
}

/// A configured machine's connector, boxed so the events and targets that carry
/// it between the loop and an attempt stay small.
type OwnedConnector = Box<shepr_remote::MachineSshConnector>;

// These variants carry different resources rather than another copy of endpoint policy: Local
// owns its resolved socket and mismatch guidance, while SSH owns a connector returned by an
// attempt.
enum ConnectTarget {
    /// The Local server socket, and the guidance a build mismatch on
    /// it names: the plain `shepr` and `shepr stop` commands, plus the
    /// socket override in effect. Resolved once from the client's paths, so the diagnostic every retry shows is the one the launch check
    /// would have printed.
    Local {
        path: PathBuf,
        mismatch_guidance: Arc<str>,
    },
    /// One connector per configured machine with the same target: it carries the
    /// launch-time ssh settings, the temporary ssh config and the remembered remote
    /// executable from one attempt to the next. An attempt takes ownership and returns it
    /// in its event.
    Ssh { connector: Option<OwnedConnector> },
}

// The attempt takes ownership of an SSH connector while it runs in a blocking task; this is an
// ownership shape distinct from the endpoint's stable policy.
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

// These clocks schedule connection attempts, not presentation availability. A successful
// handshake starts the stability clock before its first snapshot arrives; the shell's
// endpoint state alone decides whether the presentation is usable and what status to draw.
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
    /// The newest generation issued: the reserved launch generation until a
    /// background attempt takes its successor.
    last_generation: shepr_protocol::ConnectionGeneration,
    shutdown: Arc<AtomicBool>,
}

impl EndpointSupervisors {
    /// Reserves the first generation for the foreground Local attempt at launch.
    /// Background attempts start at its successor, including when Local failed.
    pub(crate) const fn initial_local_generation() -> shepr_protocol::ConnectionGeneration {
        shepr_protocol::ConnectionGeneration::FIRST
    }

    /// Supervises one SSH endpoint per connector, keyed by its machine label. The
    /// connectors come from startup preflight (carrying the transport and remote
    /// executable it verified) or from [`Self::fresh_connectors`] when no preflight ran.
    pub(crate) fn new(
        connectors: Vec<shepr_remote::MachineSshConnector>,
        now: Instant,
    ) -> io::Result<Self> {
        let mut supervisors = Self {
            endpoints: HashMap::new(),
            last_generation: Self::initial_local_generation(),
            shutdown: Arc::new(AtomicBool::new(false)),
        };
        for connector in connectors {
            let connector = Box::new(connector);
            if let Some(error) = connector.launch_fatal_setup_error() {
                return Err(error);
            }
            supervisors.endpoints.insert(
                ClientEndpointId::Ssh(connector.label().clone()),
                ReconnectState::new(
                    ConnectTarget::Ssh {
                        connector: Some(connector),
                    },
                    now,
                ),
            );
        }
        Ok(supervisors)
    }

    /// Connectors for a launch that ran no preflight: each starts with no verified
    /// transport or executable.
    pub(crate) fn fresh_connectors(
        paths: &shepr_paths::AppPaths,
        machines: &[shepr_config::MachineConfig],
    ) -> Vec<shepr_remote::MachineSshConnector> {
        machines
            .iter()
            .map(|machine| {
                shepr_remote::MachineSshConnector::new(paths, &machine.label, &machine.ssh)
            })
            .collect()
    }

    pub(crate) fn add_local(
        &mut self,
        path: PathBuf,
        mismatch_guidance: Arc<str>,
        generation: Option<shepr_protocol::ConnectionGeneration>,
        now: Instant,
    ) {
        let mut state = ReconnectState::new(
            ConnectTarget::Local {
                path,
                mismatch_guidance,
            },
            now,
        );
        state.generation = generation;
        if generation.is_some() {
            state.next_attempt = None;
        }
        self.endpoints.insert(ClientEndpointId::Local, state);
    }

    pub(crate) fn spawn_due(
        &mut self,
        now: Instant,
        make_options: impl Fn() -> EndpointConnectOptions,
        event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    ) {
        // Built only when an attempt is due: deriving the shell layout on
        // every loop pass would repeat that work for nothing.
        let mut options = None;
        for (endpoint_id, state) in &mut self.endpoints {
            if state.in_flight || state.next_attempt.is_none_or(|deadline| deadline > now) {
                continue;
            }
            // Generations tell a live attempt's events from a stale one's, so
            // a generation is never reused. Running out is unreachable (one per
            // connection attempt); should it happen, the endpoint stops
            // retrying rather than issuing a duplicate.
            let Some(generation) = self.last_generation.checked_next() else {
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
            let options = *options.get_or_insert_with(&make_options);
            state.in_flight = true;
            state.attempt_started = Some(now);
            state.next_attempt = None;
            state.generation = Some(generation);
            self.last_generation = generation;
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
                // its event. A panicking attempt loses it, but a panic ends
                // the whole client (see `fatal_panic`), so the join error
                // below only has to report the attempt without one.
                let reader_event_tx = event_tx.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let mut target = target;
                    let result = connect_once(
                        &mut target,
                        options,
                        endpoint_id,
                        generation,
                        deadline,
                        &reader_event_tx,
                    );
                    (target.into_saved_connector(), result)
                })
                .await;
                let event = match result {
                    Ok((connector, Ok(event))) => event.with_connector(connector),
                    Ok((connector, Err(error))) => {
                        let failure = shepr_launch::EndpointFailure::from_error(&error);
                        EndpointSupervisorEvent::Status {
                            endpoint_id: task_endpoint_id,
                            generation,
                            status: EndpointFailureStatus::after_failure(&failure),
                            message: failure,
                            connector,
                        }
                    }
                    Err(error) => {
                        let failure = shepr_launch::EndpointFailure::local_setup(format!(
                            "endpoint connection task stopped unexpectedly: {error}"
                        ));
                        EndpointSupervisorEvent::Status {
                            endpoint_id: task_endpoint_id,
                            generation,
                            status: EndpointFailureStatus::after_failure(&failure),
                            message: failure,
                            connector: None,
                        }
                    }
                };
                if !shutdown.load(Ordering::Acquire) {
                    // The send fails only once the client loop has exited and dropped its
                    // receiver; the returned event then drops here, releasing any
                    // connection it carries, which is all teardown needs.
                    event_tx
                        .send(ClientLoopEvent::EndpointSupervisor(event))
                        .await
                        .ok();
                }
            });
        }
    }

    /// When `spawn_due` next has an attempt to start. It skips the same states `spawn_due`
    /// skips, so a due attempt it cannot start never becomes a deadline the loop spins on.
    pub(crate) fn next_retry_deadline(&self) -> Option<Instant> {
        self.endpoints
            .values()
            .filter(|state| !state.in_flight)
            .filter(|state| {
                !matches!(
                    state.target,
                    ConnectTarget::Ssh {
                        connector: None,
                        ..
                    }
                )
            })
            .filter_map(|state| state.next_attempt)
            .min()
    }

    /// Hands a finished attempt's connector back to its endpoint. Only a
    /// panicked attempt comes back without one, and a panic ends the whole
    /// client (see `fatal_panic`), so nothing is rebuilt. An attempt of an
    /// endpoint no longer supervised drops it.
    pub(crate) fn return_connector(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
        connector: Option<OwnedConnector>,
    ) {
        let Some(state) = self.endpoints.get_mut(endpoint_id) else {
            return;
        };
        if state.generation != Some(generation) {
            return;
        }
        let ConnectTarget::Ssh { connector: owned } = &mut state.target else {
            return;
        };
        if owned.is_none() {
            *owned = connector;
        }
    }

    pub(crate) fn record_status(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
        status: ClientEndpointStatus,
        now: Instant,
    ) -> bool {
        let Some(state) = self.endpoints.get_mut(endpoint_id) else {
            return false;
        };
        if state.generation != Some(generation) {
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
                // Local's attempts reset on every Online, unlike SSH's stable-period
                // rule, so that reaching a Local server again after an outage
                // retries quickly. A Local server that accepts and then dies
                // repeatedly is retried every INITIAL_RETRY_DELAY, but only an
                // external respawner can produce that (nothing in shepr restarts
                // the server), and a retry costs one socket connect plus a
                // handshake, never a server launch. A server that is gone fails
                // the attempt and backs off normally.
                if endpoint_id.policy().resets_attempts_on_online() {
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
    generation: shepr_protocol::ConnectionGeneration,
    deadline: Instant,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
) -> Result<EndpointSupervisorEvent, std::io::Error> {
    match target {
        AttemptTarget::Local {
            path,
            mismatch_guidance,
        } => {
            let remaining = attempt_time_remaining(deadline)?;
            let stream = shepr_platform::ipc::connect_trusted_local_stream_within(path, remaining)
                .map_err(|error| {
                    // This boundary knows that an absent socket is a local server outage.
                    if error.kind() == std::io::ErrorKind::NotFound {
                        std::io::Error::new(
                            error.kind(),
                            shepr_launch::EndpointFailure::retry(
                                "the local server is unavailable; start it to reconnect",
                            ),
                        )
                    } else {
                        error
                    }
                })?;
            establish(
                stream.into_local_stream(),
                EndpointLink::Local {
                    mismatch_guidance: mismatch_guidance.as_ref(),
                },
                options,
                endpoint_id,
                generation,
                deadline,
                event_tx,
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
                event_tx,
            )
        }),
    }
}

fn attempt_time_remaining(deadline: Instant) -> Result<Duration, std::io::Error> {
    // clock-io-ok: what is left of the attempt's budget bounds the socket connect.
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "endpoint connection attempt deadline passed",
        ));
    }
    Ok(remaining)
}

/// Connection-owned state needed after connect: SSH keeps its bridge for stderr and lifetime,
/// while Local carries the mismatch guidance available from its local launch check. This is
/// separate from EndpointPolicy, which selects behavior from the endpoint identity.
enum EndpointLink<'a> {
    Local { mismatch_guidance: &'a str },
    Ssh(shepr_remote::MachineSshBridge),
}

/// Handshakes over a fresh endpoint stream and hands the connection to the loop.
fn establish(
    stream: shepr_platform::ipc::LocalStream,
    link: EndpointLink<'_>,
    options: EndpointConnectOptions,
    endpoint_id: ClientEndpointId,
    generation: shepr_protocol::ConnectionGeneration,
    deadline: Instant,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
) -> Result<EndpointSupervisorEvent, std::io::Error> {
    // The link carries the SSH bridge's lifetime and diagnostics, or Local's mismatch guidance.
    let (ssh_bridge, mismatch_guidance) = match link {
        EndpointLink::Local { mismatch_guidance } => (None, Some(mismatch_guidance)),
        EndpointLink::Ssh(bridge) => (Some(bridge), None),
    };
    let attached = crate::endpoint::connection_io::attach_endpoint_stream(
        stream,
        shepr_protocol::endpoint::EndpointClientHello {
            geometry: options.geometry,
            mouse_capture: options.mouse_capture,
            // A recovery connection stays hidden until a move requests its surface.
            surface_active: false,
        },
        endpoint_id.policy(),
        Some(deadline),
        mismatch_guidance,
        ssh_bridge,
    )?;
    let connection = crate::endpoint::connection_io::EndpointConnectionIo::start(
        attached,
        event_tx,
        endpoint_id.clone(),
        generation,
    )?;
    Ok(EndpointSupervisorEvent::Connected {
        endpoint_id,
        generation,
        connection,
        connector: None,
    })
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

    pub(crate) fn disconnected(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_generation as generation;

    fn machine() -> shepr_config::MachineConfig {
        shepr_config::MachineConfig {
            label: shepr_config::MachineLabel::parse("Build").expect("test precondition"),
            ssh: shepr_config::SshTarget::parse("build").expect("test precondition"),
            palette: None,
        }
    }

    /// Paths whose XDG runtime directory is as short as a real `/run/user/<uid>` and does
    /// not exist. These tests never connect, and no directory under the build tree is short
    /// enough for the SSH control socket's staging path. The missing directory fails the
    /// connector's runtime directory check with a plain `NotFound`, which is transient, not
    /// launch-fatal, and it is checked before any path length, so nothing is created or bound
    /// and the production launch check is untouched. Nothing reads the developer's config.
    fn short_runtime_paths() -> shepr_paths::AppPaths {
        shepr_paths::AppPaths::rooted_at(
            std::path::Path::new("/nonexistent/shepr-supervisor-tests"),
            None,
            None,
        )
        .expect("short test root")
    }

    /// `_env` is held by the caller only to keep the process environment isolated.
    fn supervisors_for(
        _env: &shepr_test_support::IsolatedEnv,
        machines: &[shepr_config::MachineConfig],
        now: Instant,
    ) -> EndpointSupervisors {
        EndpointSupervisors::new(
            EndpointSupervisors::fresh_connectors(&short_runtime_paths(), machines),
            now,
        )
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
        state.generation = Some(generation(2));
        let ConnectTarget::Ssh { connector, .. } = &mut state.target else {
            panic!("a configured machine must have an SSH target");
        };
        let connector = connector.take().expect("test connector is present");
        let has_connector = |supervisors: &EndpointSupervisors| {
            matches!(
                &supervisors.endpoints[&id].target,
                ConnectTarget::Ssh {
                    connector: Some(_),
                    ..
                }
            )
        };
        assert!(!has_connector(&supervisors));

        // The current generation's connector goes back to its endpoint.
        supervisors.return_connector(&id, generation(2), Some(connector));
        assert!(has_connector(&supervisors));

        // A stale generation's connector is dropped, not installed.
        let ConnectTarget::Ssh { connector, .. } = &mut supervisors
            .endpoints
            .get_mut(&id)
            .expect("test precondition")
            .target
        else {
            panic!("a configured machine must have an SSH target");
        };
        let connector = connector.take().expect("connector was returned");
        supervisors.return_connector(&id, generation(3), Some(connector));
        assert!(!has_connector(&supervisors));

        // Only a panicked attempt returns nothing, and a panic ends the
        // client, so nothing is rebuilt.
        supervisors.return_connector(&id, generation(2), None);
        assert!(!has_connector(&supervisors));
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
            .generation = Some(generation(2));
        for attempt in 1..=5 {
            let connected = now + Duration::from_secs(attempt * 20);
            assert!(supervisors.record_status(
                &id,
                generation(2),
                ClientEndpointStatus::Online,
                connected
            ));
            let failed = connected + Duration::from_secs(15);
            assert!(supervisors.disconnected(&id, generation(2), failed));
            assert_eq!(
                supervisors.endpoints[&id].next_attempt,
                Some(failed + INITIAL_RETRY_DELAY * (1 << (attempt - 1)))
            );
        }
        let connected = now + Duration::from_secs(200);
        assert!(supervisors.record_status(
            &id,
            generation(2),
            ClientEndpointStatus::Online,
            connected
        ));
        let failed = connected + Duration::from_secs(60);
        assert!(supervisors.disconnected(&id, generation(2), failed));
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
            .generation = Some(generation(2));
        for _ in 0..20 {
            assert!(supervisors.disconnected(&id, generation(2), now));
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
            .generation = Some(generation(9));

        assert!(supervisors.record_status(
            &id,
            generation(9),
            ClientEndpointStatus::Attention,
            now
        ));
        assert_eq!(
            supervisors.endpoints[&id].next_attempt,
            Some(now + ATTENTION_RETRY_DELAY)
        );
        assert!(supervisors.supervises(&id));

        let retry_at = now + ATTENTION_RETRY_DELAY;
        assert!(supervisors.record_status(
            &id,
            generation(9),
            ClientEndpointStatus::Reconnecting,
            retry_at,
        ));
        assert_eq!(
            supervisors.endpoints[&id].next_attempt,
            Some(retry_at + INITIAL_RETRY_DELAY)
        );
        assert!(supervisors.supervises(&id));
    }

    #[test]
    fn next_retry_deadline_exposes_due_and_backoff_attempts() {
        let env = shepr_test_support::IsolatedEnv::new();
        let now = Instant::now();
        let machine = machine();
        let id = ClientEndpointId::Ssh(machine.label.clone());
        let mut supervisors = supervisors_for(&env, &[machine], now);

        assert_eq!(supervisors.next_retry_deadline(), Some(now));

        {
            let state = supervisors
                .endpoints
                .get_mut(&id)
                .expect("test precondition");
            state.generation = Some(generation(9));
            state.in_flight = true;
        }
        assert_eq!(supervisors.next_retry_deadline(), None);
        supervisors
            .endpoints
            .get_mut(&id)
            .expect("test precondition")
            .in_flight = false;
        assert!(supervisors.record_status(
            &id,
            generation(9),
            ClientEndpointStatus::Attention,
            now
        ));
        assert_eq!(
            supervisors.next_retry_deadline(),
            Some(now + ATTENTION_RETRY_DELAY)
        );
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
                state.generation = Some(generation(7));
            }
            let promised_at = started + Duration::from_millis(10);
            let gave_up = started + ATTEMPT_BUDGET;
            assert!(supervisors.record_status(&id, generation(7), status, gave_up));
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
        let timeout = crate::errors::HandshakeError::ConnectionLost(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out",
        ))
        .class(None);
        assert!(
            !shepr_launch::EndpointFailure::from_error(&timeout)
                .disposition()
                .needs_attention()
        );
        let rejected = crate::errors::HandshakeError::HandshakeRejected {
            error: shepr_protocol::HandshakeRefusal::InvalidSurface(
                shepr_protocol::SurfaceRefusal::CellTooLarge,
            ),
        }
        .class(None);
        assert_eq!(rejected.kind(), std::io::ErrorKind::Unsupported);
        assert!(
            shepr_launch::EndpointFailure::from_error(&rejected)
                .disposition()
                .needs_attention()
        );
    }

    #[test]
    fn a_full_server_refusal_is_retried_and_names_the_limit() {
        let full = crate::errors::HandshakeError::HandshakeRejected {
            error: shepr_protocol::HandshakeRefusal::ConnectionLimit(
                shepr_protocol::LimitExceeded::new(
                    shepr_protocol::Limit::new(shepr_protocol::LimitKind::ConnectionCount, 64),
                    65,
                ),
            ),
        }
        .class(None);
        assert_eq!(full.kind(), std::io::ErrorKind::ConnectionAborted);
        assert!(
            !shepr_launch::EndpointFailure::from_error(&full)
                .disposition()
                .needs_attention()
        );
        assert!(
            full.to_string().contains("limit of 64 client connections"),
            "{full}"
        );
    }

    #[test]
    fn a_starting_server_refusal_is_retried() {
        let starting = crate::errors::HandshakeError::HandshakeRejected {
            error: shepr_protocol::HandshakeRefusal::ServerStarting,
        }
        .class(None);
        assert_eq!(starting.kind(), std::io::ErrorKind::ConnectionAborted);
        assert!(
            !shepr_launch::EndpointFailure::from_error(&starting)
                .disposition()
                .needs_attention()
        );
    }

    #[test]
    fn early_end_of_stream_and_shutdown_during_handshake_are_transient() {
        let eof =
            crate::errors::HandshakeError::Protocol(shepr_protocol::FramingError::UnexpectedEof)
                .class(None);
        assert_eq!(eof.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(
            !shepr_launch::EndpointFailure::from_error(&eof)
                .disposition()
                .needs_attention()
        );
        let shutdown = crate::errors::HandshakeError::ServerShutdown {
            reason: shepr_protocol::ShutdownReason::Stopping,
        }
        .class(None);
        assert!(
            !shepr_launch::EndpointFailure::from_error(&shutdown)
                .disposition()
                .needs_attention()
        );
        let malformed = crate::errors::HandshakeError::Protocol(
            shepr_protocol::FramingError::LimitExceeded(shepr_protocol::LimitExceeded::new(
                shepr_protocol::Limit::new(shepr_protocol::LimitKind::MessageBytes, 1),
                2,
            )),
        )
        .class(None);
        assert!(
            shepr_launch::EndpointFailure::from_error(&malformed)
                .disposition()
                .needs_attention()
        );
    }

    /// A welcome that does not decode surfaces as a handshake failure
    /// that needs attention: the endpoint is marked and the others keep running.
    #[test]
    fn a_welcome_that_does_not_decode_needs_attention() {
        let error = crate::errors::HandshakeError::Protocol(shepr_protocol::FramingError::Codec(
            shepr_protocol::codec::CodecError::InvalidUtf8,
        ))
        .class(None);
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let diagnostic = shepr_launch::EndpointFailure::from_error(&error);
        assert!(diagnostic.disposition().needs_attention());
        assert!(diagnostic.to_string().contains("handshake failed"));
    }

    fn different_build() -> crate::errors::HandshakeError {
        crate::errors::HandshakeError::Preamble(
            shepr_protocol::preamble::PreambleError::DifferentBuild(
                shepr_protocol::preamble::PeerBuild {
                    build_id: "00000000deadbeef"
                        .parse()
                        .expect("canonical build fingerprint"),
                },
            ),
        )
    }

    /// With configured machines the launch check's refusal cannot fail the launch,
    /// so the Local endpoint's own diagnostic carries it: both builds, the
    /// forced stop and the plain attach command.
    #[test]
    fn a_local_build_mismatch_names_the_restart_guidance() {
        let _env = shepr_test_support::IsolatedEnv::new();
        let paths = shepr_paths::AppPaths::resolve().expect("isolated paths resolve");
        let now = Instant::now();
        let mut supervisors = EndpointSupervisors::new(Vec::new(), now).expect("test precondition");
        let guidance: Arc<str> =
            shepr_launch::guidance::build_mismatch_guidance(paths.server_address()).into();
        supervisors.add_local(paths.server_address().socket().into(), guidance, None, now);
        let ConnectTarget::Local {
            mismatch_guidance, ..
        } = &supervisors.endpoints[&ClientEndpointId::Local].target
        else {
            panic!("Local must have a Local target");
        };

        let error = different_build().class(Some(&**mismatch_guidance));
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        let diagnostic = shepr_launch::EndpointFailure::from_error(&error);
        assert!(diagnostic.disposition().needs_attention());
        let message = diagnostic.to_string();
        // The commands name this build's own entry point, which for a test
        // build is its running executable rather than the installed `shepr`.
        let entrypoint = shepr_launch::guidance::operator_entrypoint();
        for expected in [
            "handshake failed".to_owned(),
            "00000000deadbeef".to_owned(),
            shepr_protocol::BUILD_ID.to_owned(),
            format!("`{entrypoint} stop`"),
            format!("`{entrypoint}`"),
        ] {
            assert!(message.contains(&expected), "{expected}: {message}");
        }
        assert!(!message.contains('\n'), "{message}");
    }

    /// A configured machine's mismatch keeps the generic preamble text; its bridge
    /// and remote checks report the machine-specific way out.
    #[test]
    fn a_machine_build_mismatch_keeps_the_preamble_text() {
        let error = different_build().class(None);
        assert!(error.to_string().contains("Install the same shepr build"));
        assert!(error.to_string().contains("handshake failed"));
    }

    #[test]
    fn local_in_attention_is_retried() {
        let env = shepr_test_support::IsolatedEnv::new();
        let now = Instant::now();
        let mut supervisors = supervisors_for(&env, &[], now);
        supervisors.add_local(
            PathBuf::from("local.sock"),
            Arc::from(""),
            Some(generation(1)),
            now,
        );
        assert!(supervisors.record_status(
            &ClientEndpointId::Local,
            generation(1),
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
        supervisors.add_local(
            PathBuf::from("local.sock"),
            Arc::from(""),
            Some(generation(1)),
            now,
        );
        assert!(
            supervisors.endpoints[&ClientEndpointId::Local]
                .next_attempt
                .is_none()
        );
        assert!(!supervisors.disconnected(&ClientEndpointId::Local, generation(0), now));
        assert!(
            supervisors.endpoints[&ClientEndpointId::Local]
                .next_attempt
                .is_none()
        );
        assert!(supervisors.disconnected(&ClientEndpointId::Local, generation(1), now));
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
            .generation = Some(generation(4));
        assert!(supervisors.record_status(
            &endpoint_id,
            generation(4),
            ClientEndpointStatus::Online,
            now
        ));
        assert!(!supervisors.disconnected(&endpoint_id, generation(3), now));
        assert!(supervisors.endpoints[&endpoint_id].next_attempt.is_none());
        assert!(supervisors.disconnected(&endpoint_id, generation(4), now));
        assert_eq!(
            supervisors.endpoints[&endpoint_id].next_attempt,
            Some(now + INITIAL_RETRY_DELAY)
        );
        assert!(supervisors.record_status(
            &endpoint_id,
            generation(4),
            ClientEndpointStatus::Attention,
            now
        ));
        assert_eq!(
            supervisors.endpoints[&endpoint_id].next_attempt,
            Some(now + Duration::from_secs(30))
        );
    }
}
