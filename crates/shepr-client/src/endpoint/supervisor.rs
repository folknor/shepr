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
    RESTART_ATTEMPT_BUDGET, STABLE_CONNECTION_PERIOD, START_ATTEMPT_BUDGET,
};
use shepr_remote::{ConnectMode, ServerWatchEnd};

#[derive(Clone, Copy)]
pub(crate) struct EndpointConnectOptions {
    pub(crate) geometry: shepr_protocol::TerminalGeometry,
    pub(crate) mouse_capture: bool,
}

pub(crate) enum EndpointSupervisorEvent {
    Status {
        endpoint_id: ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
        failure: shepr_launch::EndpointFailure,
        connector: Option<OwnedConnector>,
    },
    Connected {
        endpoint_id: ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
        connection: crate::endpoint::connection_io::EndpointConnectionIo,
        connector: Option<OwnedConnector>,
    },
    /// A machine's wait for its server ended: the server may be there now,
    /// the client cancelled the wait, or the wait failed (the link dropped).
    Watched {
        endpoint_id: ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
        result: Result<ServerWatchEnd, shepr_launch::EndpointFailure>,
        connector: Option<OwnedConnector>,
    },
}

impl EndpointSupervisorEvent {
    fn with_connector(self, connector: Option<OwnedConnector>) -> Self {
        match self {
            Self::Status {
                endpoint_id,
                generation,
                failure,
                ..
            } => Self::Status {
                endpoint_id,
                generation,
                failure,
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
            Self::Watched {
                endpoint_id,
                generation,
                result,
                ..
            } => Self::Watched {
                endpoint_id,
                generation,
                result,
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
        mode: ConnectMode,
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

/// What a supervisor runs for an endpoint once its next attempt comes due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scheduled {
    /// A connection attempt that only attaches: nothing the client does by
    /// itself starts a server on another host.
    Attach,
    /// A wait, on the machine, for a server to appear: for a machine that
    /// answered but runs no server. Its end schedules an attaching attempt.
    WatchForServer,
}

/// The operation a supervisor starts for an endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    Connect(ConnectMode),
    WatchForServer,
}

// These clocks schedule connection attempts, not presentation availability. A successful
// handshake starts the stability clock before its first snapshot arrives; the shell's
// endpoint state alone decides whether the presentation is usable and what status to draw.
struct ReconnectState {
    target: ConnectTarget,
    attempts: u32,
    next_attempt: Option<Instant>,
    /// What runs when `next_attempt` comes due.
    scheduled: Scheduled,
    /// An operator's Connect or Restart and when it was asked for. It runs as soon as
    /// nothing is in flight, ahead of anything scheduled.
    requested: Option<(ConnectMode, Instant)>,
    in_flight: bool,
    /// The operator's request in flight, so a stale automatic outcome does not
    /// overwrite what the machine's entry says about it.
    requested_in_flight: bool,
    /// Cancels the wait for a server in flight; `None` when no wait runs.
    watch_cancel: Option<Arc<AtomicBool>>,
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
            scheduled: Scheduled::Attach,
            requested: None,
            in_flight: false,
            requested_in_flight: false,
            watch_cancel: None,
            attempt_started: None,
            generation: None,
            online_since: None,
        }
    }

    /// The operation to start now, if any: an operator's request first, then
    /// whatever is scheduled once it is due. Taking a request consumes it.
    fn due_operation(&mut self, now: Instant) -> Option<Operation> {
        if self.in_flight {
            return None;
        }
        if let Some((mode, _)) = self.requested.take() {
            return Some(Operation::Connect(mode));
        }
        if self.next_attempt.is_none_or(|deadline| deadline > now) {
            return None;
        }
        Some(match self.scheduled {
            Scheduled::Attach => Operation::Connect(ConnectMode::Attach),
            Scheduled::WatchForServer => Operation::WatchForServer,
        })
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
            let requested = state.requested.is_some();
            let Some(operation) = state.due_operation(now) else {
                continue;
            };
            // Generations tell a live attempt's events from a stale one's, so
            // a generation is never reused. Running out is unreachable (one per
            // connection attempt); should it happen, the endpoint stops
            // retrying rather than issuing a duplicate.
            let Some(generation) = self.last_generation.checked_next() else {
                shepr_platform::structured_log!(
                    ERROR, event = endpoint.generation, outcome = "exhausted",
                    endpoint = ?endpoint_id,
                    "connection generations exhausted; not starting another attempt"
                );
                state.next_attempt = None;
                continue;
            };
            shepr_platform::structured_log!(INFO, event = endpoint.operation, outcome = "started", endpoint = %endpoint_id, %generation, mode = ?operation, "endpoint operation starting");
            let target = match (&mut state.target, operation) {
                (
                    ConnectTarget::Local {
                        path,
                        mismatch_guidance,
                    },
                    _,
                ) => AttemptTarget::Local {
                    path: path.clone(),
                    mismatch_guidance: Arc::clone(mismatch_guidance),
                },
                (ConnectTarget::Ssh { connector, .. }, Operation::Connect(mode)) => {
                    let Some(connector) = connector.take() else {
                        continue;
                    };
                    AttemptTarget::Ssh { connector, mode }
                }
                (ConnectTarget::Ssh { connector, .. }, Operation::WatchForServer) => {
                    let Some(connector) = connector.take() else {
                        continue;
                    };
                    let cancel = Arc::new(AtomicBool::new(false));
                    state.watch_cancel = Some(Arc::clone(&cancel));
                    state.in_flight = true;
                    state.requested_in_flight = false;
                    state.attempt_started = Some(now);
                    state.next_attempt = None;
                    state.generation = Some(generation);
                    self.last_generation = generation;
                    spawn_server_watch(
                        endpoint_id.clone(),
                        generation,
                        connector,
                        cancel,
                        now + ATTEMPT_BUDGET,
                        event_tx.clone(),
                        Arc::clone(&self.shutdown),
                    );
                    continue;
                }
            };
            let options = *options.get_or_insert_with(&make_options);
            state.in_flight = true;
            state.requested_in_flight = requested;
            state.attempt_started = Some(now);
            state.next_attempt = None;
            state.generation = Some(generation);
            self.last_generation = generation;
            let endpoint_id = endpoint_id.clone();
            let event_tx = event_tx.clone();
            let shutdown = Arc::clone(&self.shutdown);
            let deadline = now
                + match operation {
                    Operation::Connect(ConnectMode::Restart) => RESTART_ATTEMPT_BUDGET,
                    Operation::Connect(ConnectMode::Start) => START_ATTEMPT_BUDGET,
                    _ => ATTEMPT_BUDGET,
                };
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
                    Ok((connector, Err(error))) => EndpointSupervisorEvent::Status {
                        endpoint_id: task_endpoint_id,
                        generation,
                        failure: shepr_launch::EndpointFailure::from_error(&error),
                        connector,
                    },
                    Err(error) => EndpointSupervisorEvent::Status {
                        endpoint_id: task_endpoint_id,
                        generation,
                        failure: shepr_launch::EndpointFailure::local_setup(format!(
                            "endpoint connection task stopped unexpectedly: {error}"
                        )),
                        connector: None,
                    },
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
    /// An operator's request is due from when it was made.
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
            .filter_map(|state| {
                state
                    .requested
                    .map(|(_, requested_at)| requested_at)
                    .or(state.next_attempt)
            })
            .min()
    }

    /// Asks for an operator's Connect or Restart of a configured machine: it runs as
    /// soon as nothing is in flight for that machine, and a wait for its server in
    /// flight is cancelled for it. Refused (false) for the Local endpoint and for a
    /// machine that is connected.
    pub(crate) fn request(
        &mut self,
        endpoint_id: &ClientEndpointId,
        mode: ConnectMode,
        now: Instant,
    ) -> bool {
        let Some(state) = self.endpoints.get_mut(endpoint_id) else {
            return false;
        };
        if !matches!(state.target, ConnectTarget::Ssh { .. }) || state.online_since.is_some() {
            return false;
        }
        shepr_platform::structured_log!(INFO, event = endpoint.operation, outcome = "accepted", endpoint = %endpoint_id, generation = ?state.generation, ?mode, "endpoint operator request accepted");
        state.requested = Some((mode, now));
        if let Some(cancel) = &state.watch_cancel {
            cancel.store(true, Ordering::Release);
        }
        true
    }

    /// Whether an operator's Connect or Restart for this endpoint is waiting or in
    /// flight. An automatic outcome that lands meanwhile must not replace what the
    /// machine's entry says about the operator's request.
    pub(crate) fn request_pending(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints
            .get(endpoint_id)
            .is_some_and(|state| state.requested.is_some() || state.requested_in_flight)
    }

    /// Records a failed attempt or a lost connection, schedules what follows it,
    /// and returns the status it left, or `None` for a stale generation. A machine
    /// that answered but runs no server is not retried: a wait for its server is
    /// scheduled instead.
    pub(crate) fn record_failure(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
        failure: &shepr_launch::EndpointFailure,
        now: Instant,
    ) -> Option<EndpointFailureStatus> {
        let status = EndpointFailureStatus::after_failure(failure);
        if !self.record_status(endpoint_id, generation, status.into(), now) {
            return None;
        }
        if failure.cause() == shepr_launch::FailureCause::NoServer
            && let Some(state) = self.endpoints.get_mut(endpoint_id)
            && matches!(state.target, ConnectTarget::Ssh { .. })
        {
            // The backoff `record_status` scheduled stays: a wait that keeps ending at
            // once cannot spin.
            shepr_platform::structured_log!(INFO, event = endpoint.server_wait, outcome = "scheduled", endpoint = %endpoint_id, %generation, "endpoint wait for a server scheduled");
            state.scheduled = Scheduled::WatchForServer;
        }
        Some(status)
    }

    /// A wait for a machine's server ended. A wait that ran to its end is followed
    /// at once by an attaching attempt; a cancelled one leaves the operator's request
    /// to run. False for a stale generation.
    pub(crate) fn watch_ended(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
        end: ServerWatchEnd,
        now: Instant,
    ) -> bool {
        let Some(state) = self.endpoints.get_mut(endpoint_id) else {
            return false;
        };
        if state.generation != Some(generation) {
            return false;
        }
        shepr_platform::structured_log!(INFO, event = endpoint.server_wait, outcome = "completed", endpoint = %endpoint_id, %generation, ?end, "endpoint wait for a server ended");
        state.in_flight = false;
        state.watch_cancel = None;
        state.attempt_started = None;
        state.scheduled = Scheduled::Attach;
        match end {
            ServerWatchEnd::Ended => state.next_attempt = Some(now),
            // A cancelled wait was cancelled for an operator's request, which runs
            // next; should none be waiting, attach as after any other wait.
            ServerWatchEnd::Cancelled => {
                if state.requested.is_none() {
                    state.next_attempt = Some(now);
                }
            }
        }
        true
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
        state.requested_in_flight = false;
        state.watch_cancel = None;
        state.scheduled = Scheduled::Attach;
        match status {
            ClientEndpointStatus::Online => {
                // Connected: an operator's request still waiting has nothing to do.
                state.requested = None;
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
        // A wait for a server blocks until cancelled; the runtime's shutdown
        // must not wait on one.
        for state in self.endpoints.values() {
            if let Some(cancel) = &state.watch_cancel {
                cancel.store(true, Ordering::Release);
            }
        }
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
                                shepr_launch::guidance::LOCAL_RECONNECT_HINT,
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
        AttemptTarget::Ssh { connector, mode } => connector.connect(deadline, *mode, |connected| {
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

/// Runs a machine's wait for its server on a blocking task and reports how it
/// ended, handing the connector back with the event. `deadline` bounds only the
/// executable resolution before the wait; `cancel` ends the wait itself, and the
/// supervisors set it when they are dropped, so a runtime shutting down never
/// waits on it.
fn spawn_server_watch(
    endpoint_id: ClientEndpointId,
    generation: shepr_protocol::ConnectionGeneration,
    mut connector: OwnedConnector,
    cancel: Arc<AtomicBool>,
    deadline: Instant,
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    shutdown: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        let task_endpoint_id = endpoint_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            let result = connector
                .wait_for_server(deadline, &cancel)
                .map_err(|error| shepr_launch::EndpointFailure::from_error(&error));
            (connector, result)
        })
        .await;
        let event = match result {
            Ok((connector, result)) => EndpointSupervisorEvent::Watched {
                endpoint_id: task_endpoint_id,
                generation,
                result,
                connector: Some(connector),
            },
            Err(error) => EndpointSupervisorEvent::Watched {
                endpoint_id: task_endpoint_id,
                generation,
                result: Err(shepr_launch::EndpointFailure::local_setup(format!(
                    "the wait for the machine's server stopped unexpectedly: {error}"
                ))),
                connector: None,
            },
        };
        if !shutdown.load(Ordering::Acquire) {
            // As for a connection attempt: a send fails only once the loop is gone.
            event_tx
                .send(ClientLoopEvent::EndpointSupervisor(event))
                .await
                .ok();
        }
    });
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

/// Endpoint reconnect backoff: attempt one uses the initial delay, and each
/// later attempt doubles it up to the maximum retry delay.
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

    /// Puts an attempt of `generation` in flight, as `spawn_due` does when it starts
    /// one, without running it.
    pub(crate) fn mark_in_flight(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
        now: Instant,
    ) {
        if let Some(state) = self.endpoints.get_mut(endpoint_id) {
            state.in_flight = true;
            state.attempt_started = Some(now);
            state.next_attempt = None;
            state.generation = Some(generation);
        }
    }

    /// The operator's request waiting to run for this endpoint.
    pub(crate) fn pending_request(&self, endpoint_id: &ClientEndpointId) -> Option<ConnectMode> {
        self.endpoints
            .get(endpoint_id)
            .and_then(|state| state.requested)
            .map(|(mode, _)| mode)
    }

    /// Supervisors for `machines` whose connectors never reach a host: their
    /// runtime directory does not exist, so nothing is created or bound.
    pub(crate) fn unreachable_for_tests(
        machines: &[shepr_config::MachineConfig],
        now: Instant,
    ) -> Self {
        let paths = shepr_paths::AppPaths::rooted_at(
            std::path::Path::new("/nonexistent/shepr-supervisor-tests"),
            None,
            None,
        )
        .expect("short test root");
        Self::new(Self::fresh_connectors(&paths, machines), now)
            .expect("test saved SSH setup is retryable")
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
    use crate::limits::RETRY_PROMISE;
    use crate::tests::test_generation as generation;

    fn machine() -> shepr_config::MachineConfig {
        shepr_config::MachineConfig {
            label: shepr_config::MachineLabel::parse("Build").expect("test precondition"),
            ssh: shepr_config::SshTarget::parse("build").expect("test precondition"),
            palette: shepr_config::DEFAULT_LOCAL_HUE,
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
    fn a_reconnecting_machine_retries_within_the_retry_promise() {
        // Open clients keep a reachable machine's next attempt within the promise.
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
                    .is_some_and(|next| next <= now + RETRY_PROMISE)
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
    fn a_slow_failed_attempt_still_meets_the_retry_promise_from_any_moment() {
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
                next <= promised_at + RETRY_PROMISE,
                "{status:?}: retry {:?} after the promise",
                next.saturating_duration_since(promised_at)
            );
            // The retry delay counts from the attempt's start, not from when it gave up.
            assert!(next < gave_up + RETRY_PROMISE);
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
            Some(now + ATTENTION_RETRY_DELAY)
        );
    }

    /// A machine whose attempt is in flight at `generation`, as `spawn_due` leaves it.
    fn in_flight(
        supervisors: &mut EndpointSupervisors,
        id: &ClientEndpointId,
        position: u64,
        now: Instant,
    ) {
        let state = supervisors
            .endpoints
            .get_mut(id)
            .expect("test precondition");
        state.in_flight = true;
        state.attempt_started = Some(now);
        state.next_attempt = None;
        state.generation = Some(generation(position));
    }

    fn due(
        supervisors: &mut EndpointSupervisors,
        id: &ClientEndpointId,
        now: Instant,
    ) -> Option<Operation> {
        let state = supervisors.endpoints.get_mut(id);
        assert!(state.is_some(), "test precondition: {id} is supervised");
        state?.due_operation(now)
    }

    /// The client never starts a server by itself: a connection lost to a server that
    /// stopped is followed by an attaching attempt, which finds the server still
    /// running or finds none, and then only waits for one.
    #[test]
    fn a_dropped_connection_reattaches_and_never_starts_a_server() {
        let env = shepr_test_support::IsolatedEnv::new();
        let now = Instant::now();
        let id = ClientEndpointId::Ssh(machine().label);
        let mut supervisors = supervisors_for(&env, &[machine()], now);
        assert_eq!(
            due(&mut supervisors, &id, now),
            Some(Operation::Connect(ConnectMode::Attach))
        );

        in_flight(&mut supervisors, &id, 2, now);
        assert!(supervisors.record_status(&id, generation(2), ClientEndpointStatus::Online, now));
        // The server announces its shutdown, and the connection drops.
        let lost = now + Duration::from_secs(5);
        let shutdown = shepr_launch::EndpointFailure::server_shutdown(
            shepr_protocol::ShutdownReason::Stopping,
        );
        assert_eq!(
            supervisors.record_failure(&id, generation(2), &shutdown, lost),
            Some(EndpointFailureStatus::Reconnecting)
        );
        let retry_at = lost + MAX_RETRY_DELAY;
        assert_eq!(
            due(&mut supervisors, &id, retry_at),
            Some(Operation::Connect(ConnectMode::Attach)),
            "the reconnect only attaches"
        );

        // That attempt finds no server: the machine is watched, not retried.
        in_flight(&mut supervisors, &id, 3, retry_at);
        let none = shepr_launch::EndpointFailure::no_server("no shepr server is running");
        assert!(
            supervisors
                .record_failure(&id, generation(3), &none, retry_at)
                .is_some()
        );
        let watch_at = retry_at + MAX_RETRY_DELAY;
        assert_eq!(
            due(&mut supervisors, &id, watch_at),
            Some(Operation::WatchForServer)
        );

        // The wait ends when a server appears, and an attaching attempt follows at once.
        in_flight(&mut supervisors, &id, 4, watch_at);
        assert!(supervisors.watch_ended(&id, generation(4), ServerWatchEnd::Ended, watch_at));
        assert_eq!(
            due(&mut supervisors, &id, watch_at),
            Some(Operation::Connect(ConnectMode::Attach))
        );
    }

    #[test]
    fn connect_runs_a_starting_attempt_ahead_of_anything_scheduled() {
        let env = shepr_test_support::IsolatedEnv::new();
        let now = Instant::now();
        let id = ClientEndpointId::Ssh(machine().label);
        let mut supervisors = supervisors_for(&env, &[machine()], now);
        // A wait for the server is in flight.
        in_flight(&mut supervisors, &id, 2, now);
        let cancel = Arc::new(AtomicBool::new(false));
        supervisors
            .endpoints
            .get_mut(&id)
            .expect("test precondition")
            .watch_cancel = Some(Arc::clone(&cancel));

        let asked = now + Duration::from_secs(1);
        assert!(supervisors.request(&id, ConnectMode::Start, asked));
        assert!(cancel.load(Ordering::Acquire), "the wait is cancelled");
        assert!(supervisors.request_pending(&id));
        assert_eq!(
            due(&mut supervisors, &id, asked),
            None,
            "the wait still runs"
        );

        assert!(supervisors.watch_ended(&id, generation(2), ServerWatchEnd::Cancelled, asked));
        assert_eq!(supervisors.next_retry_deadline(), Some(asked));
        assert_eq!(
            due(&mut supervisors, &id, asked),
            Some(Operation::Connect(ConnectMode::Start))
        );
        assert_eq!(
            due(&mut supervisors, &id, asked),
            None,
            "the request runs once"
        );
    }

    #[test]
    fn restart_is_its_own_attempt_mode() {
        let env = shepr_test_support::IsolatedEnv::new();
        let now = Instant::now();
        let id = ClientEndpointId::Ssh(machine().label);
        let mut supervisors = supervisors_for(&env, &[machine()], now);
        assert!(supervisors.request(&id, ConnectMode::Restart, now));
        assert_eq!(
            due(&mut supervisors, &id, now),
            Some(Operation::Connect(ConnectMode::Restart))
        );
    }

    #[test]
    fn requests_are_refused_for_local_and_for_a_connected_machine() {
        let env = shepr_test_support::IsolatedEnv::new();
        let now = Instant::now();
        let id = ClientEndpointId::Ssh(machine().label);
        let mut supervisors = supervisors_for(&env, &[machine()], now);
        supervisors.add_local(
            PathBuf::from("local.sock"),
            Arc::from(""),
            Some(generation(1)),
            now,
        );
        assert!(!supervisors.request(&ClientEndpointId::Local, ConnectMode::Start, now));

        in_flight(&mut supervisors, &id, 2, now);
        assert!(supervisors.record_status(&id, generation(2), ClientEndpointStatus::Online, now));
        assert!(!supervisors.request(&id, ConnectMode::Start, now));
        assert_eq!(due(&mut supervisors, &id, now), None);
    }
}
