use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Instant;

use super::ClientEndpointId;
use super::connection_io::{EndpointReadActivity, NativeEndpointTransport};
use super::health::{EndpointHealth, HealthAction};
use crate::deadline::Deadline;
use crate::limits::ENDPOINT_DETACH_FLUSH_TIMEOUT;
use shepr_protocol::{ClientMessage, ConnectionGeneration};

pub(crate) trait EndpointTransport: Send {
    fn send(&mut self, message: &ClientMessage) -> io::Result<()>;

    fn disconnect(&mut self);

    fn flush(&mut self, deadline: Instant) -> io::Result<()>;

    fn take_error(&mut self) -> Option<io::Error>;
}

pub(crate) struct EndpointConnection {
    transport: Box<dyn EndpointTransport>,
    pub(crate) generation: shepr_protocol::ConnectionGeneration,
    pub(crate) viewed: bool,
    health: EndpointHealth,
    /// Frame arrivals as the reader thread stamps them; a connection that has one takes
    /// its health from it rather than from the client loop's processing. Only synthetic
    /// test transports have none.
    read_activity: Option<Arc<EndpointReadActivity>>,
    // Explicit detach paths use send_to before the registry is dropped.
    detach_sent: bool,
}

impl EndpointConnection {
    fn sync_reader_activity(&mut self) {
        if let Some(read_activity) = &self.read_activity {
            let observation = read_activity.observed();
            self.health
                .sync_reader_activity(observation.last_frame_at, observation.snapshot_seen);
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct EndpointTransportFailure {
    pub(crate) endpoint_id: ClientEndpointId,
    pub(crate) generation: ConnectionGeneration,
    pub(crate) kind: io::ErrorKind,
    pub(crate) failure: shepr_launch::EndpointFailure,
}

impl std::fmt::Display for EndpointTransportFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.failure)
    }
}

impl std::error::Error for EndpointTransportFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.failure)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndpointSendOutcome {
    Sent,
    NotSent,
}

pub(crate) struct EndpointRegistry {
    connections: HashMap<ClientEndpointId, EndpointConnection>,
    failures: Vec<EndpointTransportFailure>,
}

impl EndpointRegistry {
    pub(crate) fn empty() -> Self {
        Self {
            connections: HashMap::new(),
            failures: Vec::new(),
        }
    }

    /// A registry whose Local slot is a server socket on this host, connected at `now`.
    pub(crate) fn new_at(
        local: NativeEndpointTransport,
        generation: ConnectionGeneration,
        now: Instant,
    ) -> Self {
        let mut registry = Self::empty();
        registry.insert_native(ClientEndpointId::Local, local, generation, true, now);
        registry
    }

    /// Whether this endpoint's connection has been told it is viewed. False without a
    /// connection.
    pub(crate) fn viewed(&self, id: &ClientEndpointId) -> bool {
        self.connections
            .get(id)
            .is_some_and(|connection| connection.viewed)
    }

    pub(crate) fn connection(&self, endpoint_id: &ClientEndpointId) -> Option<&EndpointConnection> {
        self.connections.get(endpoint_id)
    }

    /// `viewed` is what the connection's hello told its server (`surface_active`),
    /// as `set_viewed` later records. A bool rather than an enum: it is the only
    /// bool among these parameters, so a call site cannot swap it with another,
    /// and it mirrors the wire field and the `viewed` query.
    pub(crate) fn insert_native(
        &mut self,
        endpoint_id: ClientEndpointId,
        transport: NativeEndpointTransport,
        generation: ConnectionGeneration,
        viewed: bool,
        now: Instant,
    ) {
        let read_activity = Some(transport.read_activity());
        self.insert_with_activity(
            endpoint_id,
            transport,
            generation,
            viewed,
            read_activity,
            now,
        );
    }

    fn insert_with_activity(
        &mut self,
        endpoint_id: ClientEndpointId,
        transport: impl EndpointTransport + 'static,
        generation: ConnectionGeneration,
        viewed: bool,
        read_activity: Option<Arc<EndpointReadActivity>>,
        now: Instant,
    ) {
        // Every endpoint, local or SSH, keeps a heartbeat: a server that is alive but wedged
        // (stopped, or its connection stuck) leaves its socket open and silent, which no
        // transport error reports. Readers timestamp complete frames before queueing them;
        // the client loop handles ready queue entries (up to a bounded allowance) before an
        // expired health deadline, so shared-queue pressure does not expire an endpoint
        // whose reply is already waiting to be handled.
        if let Some(mut previous) = self.connections.insert(
            endpoint_id,
            EndpointConnection {
                transport: Box::new(transport),
                generation,
                viewed,
                health: EndpointHealth::new(now),
                read_activity,
                detach_sent: false,
            },
        ) {
            previous.transport.disconnect();
        }
    }

    pub(crate) fn accepts(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: ConnectionGeneration,
    ) -> bool {
        self.connections
            .get(endpoint_id)
            .is_some_and(|connection| connection.generation == generation)
    }

    pub(crate) fn mark_ready(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: ConnectionGeneration,
    ) {
        if let Some(connection) = self
            .connections
            .get_mut(endpoint_id)
            .filter(|connection| connection.generation == generation)
        {
            connection.health.ready();
        }
    }

    /// When the client loop must next run `tick_health` and `take_failures`: at `now` while a
    /// transport failure is queued (a send can fail during any event, and the loop learns of
    /// the loss only from that entry), otherwise at the earliest heartbeat deadline. A writer
    /// error stored while the loop sleeps surfaces through the reader's disconnect or the next
    /// send, which fails once the writer has stopped.
    pub(crate) fn next_service_deadline(&mut self, now: Instant) -> Option<Instant> {
        if !self.failures.is_empty() {
            return Some(now);
        }
        self.connections
            .values_mut()
            .map(|connection| {
                connection.sync_reader_activity();
                connection.health.next_deadline()
            })
            .min()
    }

    pub(crate) fn tick_health(&mut self, now: Instant) {
        let actions = self
            .connections
            .iter_mut()
            .map(|(endpoint_id, connection)| {
                connection.sync_reader_activity();
                (endpoint_id.clone(), connection.health.action(now))
            })
            .filter(|(_, action)| *action != HealthAction::None)
            .collect::<Vec<_>>();
        for (endpoint_id, action) in actions {
            match action {
                HealthAction::None => {}
                HealthAction::Ping => {
                    let ping = ClientMessage::HealthPing;
                    if self.send_to(&endpoint_id, &ping) == EndpointSendOutcome::Sent
                        && let Some(connection) = self.connections.get_mut(&endpoint_id)
                    {
                        // Native transports queue the frame before this marker, so reader stamps
                        // observed during the earlier sync remain pre-probe activity.
                        // clock-io-ok: the reader thread stamps frames with the real clock.
                        connection.health.ping_sent(Instant::now().max(now));
                    }
                }
                HealthAction::Expired => self.record_failure(
                    &endpoint_id,
                    &io::Error::new(io::ErrorKind::TimedOut, "endpoint health check timed out"),
                ),
            }
        }
    }

    /// Records whether the connection has been told it is viewed; returns whether that changed.
    /// Outside tests only `view::turn_on` sets it, right before it sends the on request;
    /// `release_unwanted_views` clears it the same way before the off request.
    pub(crate) fn set_viewed(&mut self, endpoint_id: &ClientEndpointId, viewed: bool) -> bool {
        let Some(connection) = self.connections.get_mut(endpoint_id) else {
            return false;
        };
        let changed = connection.viewed != viewed;
        connection.viewed = viewed;
        changed
    }

    /// Sends to every viewed connection (a resize, a theme update). Sends while iterating; a
    /// failed send's endpoint id is collected (the only allocation, on the failure path) and
    /// recorded after the traversal, because `record_failure` removes from the map being
    /// iterated.
    pub(crate) fn send_viewed(&mut self, message: &ClientMessage) {
        let mut failures = Vec::new();
        for (id, connection) in &mut self.connections {
            if connection.viewed
                && let Err(error) = connection.transport.send(message)
            {
                failures.push((id.clone(), error));
            }
        }
        for (id, error) in failures {
            self.record_failure(&id, &error);
        }
    }

    /// One pass over the connections: every viewed connection that is not `wanted` and whose
    /// boot id `boot_id_of` knows is marked not viewed, then sent focus-loss and the view-off
    /// request with an independently allocated identity. The
    /// off acknowledgement is never awaited. A connection without a known boot id is skipped
    /// and stays viewed for the next pass. Failed sends are recorded after the traversal as in
    /// `send_viewed`. Returns how many connections were released. Allocates nothing when no
    /// connection is viewed and unwanted.
    pub(crate) fn release_unwanted_views<'a>(
        &mut self,
        wanted: impl Fn(&ClientEndpointId) -> bool,
        boot_id_of: impl Fn(&ClientEndpointId) -> Option<&'a shepr_protocol::BootId>,
    ) -> usize {
        let mut failures = Vec::new();
        let mut released = 0;
        for (id, connection) in &mut self.connections {
            if !connection.viewed || wanted(id) {
                continue;
            }
            let Some(boot) = boot_id_of(id) else {
                continue;
            };
            connection.viewed = false;
            released += 1;
            let request_id = shepr_protocol::RequestId::allocate();
            let request = super::view::surface_interest_request(boot, request_id, false);
            // A transport that failed the focus-loss is a lost connection; its server drops the
            // view with it, so the release is not sent after it.
            let sent = connection
                .transport
                .send(&ClientMessage::ClientShellFocus { focused: false })
                .and_then(|()| connection.transport.send(&request));
            if let Err(error) = sent {
                failures.push((id.clone(), error));
            }
        }
        for (id, error) in failures {
            self.record_failure(&id, &error);
        }
        released
    }

    pub(crate) fn send_to(
        &mut self,
        endpoint_id: &ClientEndpointId,
        message: &ClientMessage,
    ) -> EndpointSendOutcome {
        let is_detach = matches!(message, ClientMessage::Detach);
        if is_detach
            && self
                .connections
                .get(endpoint_id)
                .is_some_and(|connection| connection.detach_sent)
        {
            return EndpointSendOutcome::Sent;
        }
        let result = self
            .connections
            .get_mut(endpoint_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "endpoint is unavailable"))
            .and_then(|connection| connection.transport.send(message));
        match result {
            Ok(()) => {
                if is_detach && let Some(connection) = self.connections.get_mut(endpoint_id) {
                    connection.detach_sent = true;
                }
                EndpointSendOutcome::Sent
            }
            Err(error) => {
                self.record_failure(endpoint_id, &error);
                EndpointSendOutcome::NotSent
            }
        }
    }

    pub(crate) fn fail(&mut self, endpoint_id: &ClientEndpointId, error: &io::Error) {
        self.record_failure(endpoint_id, error);
    }

    pub(crate) fn take_failures(&mut self) -> Vec<EndpointTransportFailure> {
        let errors = self
            .connections
            .iter_mut()
            .filter_map(|(id, connection)| {
                connection
                    .transport
                    .take_error()
                    .map(|error| (id.clone(), error))
            })
            .collect::<Vec<_>>();
        for (endpoint_id, error) in errors {
            self.record_failure(&endpoint_id, &error);
        }
        std::mem::take(&mut self.failures)
    }

    fn record_failure(&mut self, endpoint_id: &ClientEndpointId, error: &io::Error) {
        let Some(mut connection) = self.connections.remove(endpoint_id) else {
            return;
        };
        let writer_error = connection.transport.take_error();
        connection.transport.disconnect();
        let error = writer_error.as_ref().unwrap_or(error);
        let failure = EndpointTransportFailure {
            endpoint_id: endpoint_id.clone(),
            generation: connection.generation,
            kind: error.kind(),
            failure: shepr_launch::EndpointFailure::from_error(error),
        };
        if let Some(existing) = self
            .failures
            .iter_mut()
            .find(|existing| existing.endpoint_id == *endpoint_id)
        {
            *existing = failure;
        } else {
            // One entry per endpoint, and only for an endpoint that had a
            // connection, so the endpoint set bounds this list. It must not be
            // capped by dropping entries: the connection is already removed
            // above, and the loop learns of the loss only from this entry.
            self.failures.push(failure);
        }
    }
}

impl Drop for EndpointRegistry {
    fn drop(&mut self) {
        // clock-io-ok: bound the best-effort Detach flush during shutdown.
        let deadline = Deadline::after(Instant::now(), ENDPOINT_DETACH_FLUSH_TIMEOUT);
        // Send one courtesy Detach per connection. An interactive detach may already have
        // queued it, in which case the drop path only flushes and disconnects that connection.
        // A server also treats the closed connection as this client leaving, so a Detach that
        // fails to send or flush changes nothing.
        for connection in self.connections.values_mut() {
            if !connection.detach_sent {
                connection.transport.send(&ClientMessage::Detach).ok();
                connection.detach_sent = true;
            }
        }
        for connection in self.connections.values_mut() {
            connection.transport.flush(deadline.instant()).ok();
            connection.transport.disconnect();
        }
    }
}

#[cfg(test)]
impl EndpointRegistry {
    /// Synthetic transports have no reader thread to stamp arrivals for health tests.
    pub(crate) fn received(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: ConnectionGeneration,
        now: Instant,
    ) {
        if let Some(connection) = self
            .connections
            .get_mut(endpoint_id)
            .filter(|connection| connection.generation == generation)
            .filter(|connection| connection.read_activity.is_none())
        {
            connection.health.received(now);
        }
    }

    /// Inserts a synthetic transport, which has no reader stamps: its health
    /// follows `received` instead.
    pub(crate) fn insert(
        &mut self,
        endpoint_id: ClientEndpointId,
        transport: impl EndpointTransport + 'static,
        generation: ConnectionGeneration,
        viewed: bool,
        now: Instant,
    ) {
        self.insert_with_activity(endpoint_id, transport, generation, viewed, None, now);
    }

    /// [`Self::new_at`] over a synthetic Local transport.
    pub(crate) fn new_synthetic_at(
        local: impl EndpointTransport + 'static,
        generation: ConnectionGeneration,
        now: Instant,
    ) -> Self {
        let mut registry = Self::empty();
        registry.insert(ClientEndpointId::Local, local, generation, true, now);
        registry
    }

    pub(crate) fn new(
        local: impl EndpointTransport + 'static,
        generation: ConnectionGeneration,
    ) -> Self {
        // clock-io-ok: this test-only constructor stands in for the client launch.
        Self::new_synthetic_at(local, generation, Instant::now())
    }

    pub(crate) fn disconnect(&mut self, endpoint_id: &ClientEndpointId) {
        self.failures
            .retain(|failure| &failure.endpoint_id != endpoint_id);
        if let Some(mut connection) = self.connections.remove(endpoint_id) {
            connection.transport.disconnect();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::tests::test_generation as generation;

    struct FakeTransport {
        sent: Arc<Mutex<Vec<ClientMessage>>>,
        error: Option<io::ErrorKind>,
    }

    impl EndpointTransport for FakeTransport {
        fn send(&mut self, message: &ClientMessage) -> io::Result<()> {
            if let Some(kind) = self.error {
                return Err(io::Error::new(kind, "fake transport failure"));
            }
            self.sent
                .lock()
                .map_err(|_| io::Error::other("test precondition: lock poisoned"))?
                .push(message.clone());
            Ok(())
        }

        fn disconnect(&mut self) {}

        fn flush(&mut self, _deadline: Instant) -> io::Result<()> {
            Ok(())
        }

        fn take_error(&mut self) -> Option<io::Error> {
            None
        }
    }

    fn profile() -> crate::endpoint::MachineLabel {
        crate::endpoint::MachineLabel::parse("build").expect("test precondition")
    }

    #[test]
    fn live_failures_keep_their_operator_action_and_local_cause() {
        for (failure, status, notice) in [
            (
                shepr_launch::EndpointFailure::incompatible("patch baseline rejected"),
                super::super::EndpointFailureStatus::Attention,
                "connection failed; needs attention",
            ),
            (
                shepr_launch::EndpointFailure::backpressure("endpoint output queue is full"),
                super::super::EndpointFailureStatus::Reconnecting,
                "local output queue filled; reconnecting",
            ),
        ] {
            let mut registry = EndpointRegistry::new(
                FakeTransport {
                    sent: Arc::new(Mutex::new(Vec::new())),
                    error: None,
                },
                generation(1),
            );
            registry.fail(&ClientEndpointId::Local, &io::Error::other(failure));
            let failures = registry.take_failures();
            assert_eq!(failures.len(), 1);
            assert_eq!(
                super::super::EndpointFailureStatus::after_failure(&failures[0].failure),
                status
            );
            assert_eq!(failures[0].failure.disconnect_notice(), notice);
            assert!(registry.take_failures().is_empty());
        }
    }

    #[test]
    fn endpoint_failures_do_not_remove_other_connections() {
        let local_sent = Arc::new(Mutex::new(Vec::new()));
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::clone(&local_sent),
                error: None,
            },
            generation(1),
        );
        let ssh_id = ClientEndpointId::Ssh(profile());
        registry.insert(
            ssh_id.clone(),
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: Some(io::ErrorKind::BrokenPipe),
            },
            generation(2),
            true,
            Instant::now(),
        );

        assert_eq!(
            registry.send_to(&ssh_id, &ClientMessage::ClientShellFocus { focused: true }),
            EndpointSendOutcome::NotSent
        );
        assert!(registry.connection(&ssh_id).is_none());
        assert!(registry.connection(&ClientEndpointId::Local).is_some());
        assert_eq!(registry.take_failures()[0].endpoint_id, ssh_id);

        assert_eq!(
            registry.send_to(
                &ClientEndpointId::Local,
                &ClientMessage::ClientShellFocus { focused: true }
            ),
            EndpointSendOutcome::Sent
        );
        assert_eq!(local_sent.lock().expect("test precondition").len(), 1);
    }

    #[test]
    fn every_failed_endpoint_reports_its_newest_failure_once() {
        fn endpoint_id(index: usize) -> ClientEndpointId {
            ClientEndpointId::Ssh(
                crate::endpoint::MachineLabel::parse(format!("machine-{index}"))
                    .expect("test machine label"),
            )
        }
        fn insert(registry: &mut EndpointRegistry, index: usize, position: u64) {
            registry.insert(
                endpoint_id(index),
                FakeTransport {
                    sent: Arc::new(Mutex::new(Vec::new())),
                    error: None,
                },
                generation(position),
                false,
                Instant::now(),
            );
        }

        // Each failure removes its connection, so a dropped entry would lose
        // that endpoint for good: every one must come back out.
        let endpoints = 100;
        let mut registry = EndpointRegistry::empty();
        for index in 0..endpoints {
            insert(&mut registry, index, 1);
            registry.fail(
                &endpoint_id(index),
                &io::Error::new(io::ErrorKind::BrokenPipe, "first failure"),
            );
        }
        // A reconnected endpoint that fails again replaces its earlier entry.
        insert(&mut registry, 0, 2);
        registry.fail(
            &endpoint_id(0),
            &io::Error::new(io::ErrorKind::TimedOut, "second failure"),
        );

        let failures = registry.take_failures();
        assert_eq!(failures.len(), endpoints);
        for index in 0..endpoints {
            assert_eq!(
                failures
                    .iter()
                    .filter(|failure| failure.endpoint_id == endpoint_id(index))
                    .count(),
                1
            );
        }
        let first = &failures[0];
        assert_eq!(first.endpoint_id, endpoint_id(0));
        assert_eq!(
            (first.generation, first.kind),
            (generation(2), io::ErrorKind::TimedOut)
        );
    }

    #[test]
    fn a_reconnected_connection_is_not_viewed_until_told() {
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            generation(1),
        );
        let ssh_id = ClientEndpointId::Ssh(profile());
        registry.insert(
            ssh_id.clone(),
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            generation(2),
            true,
            Instant::now(),
        );
        assert!(registry.viewed(&ssh_id));
        registry.disconnect(&ssh_id);
        registry.insert(
            ssh_id.clone(),
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            generation(3),
            false,
            Instant::now(),
        );
        assert!(!registry.viewed(&ssh_id));
        registry.set_viewed(&ssh_id, true);
        assert!(registry.viewed(&ssh_id));
    }

    /// A local server that is alive but silent (stopped, or stuck) keeps its
    /// socket open, so no transport error reports it: the heartbeat probes it
    /// and expires the connection like a machine's.
    #[test]
    fn a_silent_local_server_is_probed_then_expired() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let now = Instant::now();
        let mut registry = EndpointRegistry::new_synthetic_at(
            FakeTransport {
                sent: Arc::clone(&sent),
                error: None,
            },
            generation(1),
            now,
        );
        registry.mark_ready(&ClientEndpointId::Local, generation(1));
        let ping_at = now + crate::limits::HEARTBEAT_INTERVAL;

        registry.tick_health(ping_at);
        assert!(matches!(
            sent.lock().expect("test precondition").as_slice(),
            [ClientMessage::HealthPing]
        ));
        registry.tick_health(ping_at + crate::limits::HEARTBEAT_TIMEOUT);
        assert!(registry.connection(&ClientEndpointId::Local).is_none());
        let failures = registry.take_failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].endpoint_id, ClientEndpointId::Local);
        assert_eq!(failures[0].kind, io::ErrorKind::TimedOut);
    }

    #[test]
    fn negotiated_remote_health_probe_expires_the_connection() {
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            generation(1),
        );
        let ssh_id = ClientEndpointId::Ssh(profile());
        let sent = Arc::new(Mutex::new(Vec::new()));
        registry.insert(
            ssh_id.clone(),
            FakeTransport {
                sent: Arc::clone(&sent),
                error: None,
            },
            generation(2),
            false,
            Instant::now(),
        );
        let now = Instant::now();
        registry.tick_health(now + crate::limits::HEARTBEAT_INTERVAL);
        assert!(matches!(
            sent.lock().expect("test precondition").as_slice(),
            [ClientMessage::HealthPing]
        ));

        registry.tick_health(
            now + crate::limits::HEARTBEAT_INTERVAL + crate::limits::HEARTBEAT_TIMEOUT,
        );
        assert!(registry.connection(&ssh_id).is_none());
        assert_eq!(registry.take_failures()[0].kind, io::ErrorKind::TimedOut);
    }

    #[test]
    fn a_ready_endpoint_can_stay_connected_after_the_initial_deadline() {
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            generation(1),
        );
        let ssh_id = ClientEndpointId::Ssh(profile());
        registry.insert(
            ssh_id.clone(),
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            generation(2),
            false,
            Instant::now(),
        );
        let now = Instant::now();
        registry.mark_ready(&ssh_id, generation(2));
        registry.received(
            &ssh_id,
            generation(2),
            now + crate::limits::HEARTBEAT_INTERVAL,
        );
        registry.tick_health(now + crate::limits::HEARTBEAT_TIMEOUT);
        assert!(registry.connection(&ssh_id).is_some());
    }

    fn insert_with_reader(
        registry: &mut EndpointRegistry,
        sent: &Arc<Mutex<Vec<ClientMessage>>>,
        now: Instant,
    ) -> Arc<EndpointReadActivity> {
        let activity = Arc::new(EndpointReadActivity::new(now));
        registry.insert_with_activity(
            ClientEndpointId::Ssh(profile()),
            FakeTransport {
                sent: Arc::clone(sent),
                error: None,
            },
            generation(2),
            false,
            Some(Arc::clone(&activity)),
            now,
        );
        activity
    }

    #[test]
    fn frames_stamped_by_the_reader_keep_a_stalled_loop_connected() {
        let mut registry = EndpointRegistry::empty();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let now = Instant::now();
        let activity = insert_with_reader(&mut registry, &sent, now);
        let ssh_id = ClientEndpointId::Ssh(profile());
        let ping_at = now + crate::limits::HEARTBEAT_INTERVAL;
        registry.tick_health(ping_at);
        assert!(matches!(
            sent.lock().expect("test precondition").as_slice(),
            [ClientMessage::HealthPing]
        ));

        // The snapshot and the pong arrive while the loop is stalled: the reader stamps them,
        // the loop never processes them, and the next timer wake comes long after.
        activity.record(now + Duration::from_millis(1), true);
        activity.record(ping_at + Duration::from_millis(1), false);
        registry.tick_health(ping_at + crate::limits::HEARTBEAT_TIMEOUT);
        assert!(registry.connection(&ssh_id).is_some());
        assert!(registry.take_failures().is_empty());
    }

    #[test]
    fn a_silent_reader_still_expires_the_connection() {
        let now = Instant::now();
        let ssh_id = ClientEndpointId::Ssh(profile());

        // Nothing ever arrives: the first-snapshot deadline expires it.
        let mut registry = EndpointRegistry::empty();
        let sent = Arc::new(Mutex::new(Vec::new()));
        insert_with_reader(&mut registry, &sent, now);
        registry.tick_health(now + crate::limits::HEARTBEAT_TIMEOUT);
        assert!(registry.connection(&ssh_id).is_none());
        assert_eq!(registry.take_failures()[0].kind, io::ErrorKind::TimedOut);

        // A snapshot arrived, then the link went quiet: the unanswered ping expires it, and
        // frames the loop reports processing do not stand in for the reader's stamps.
        let mut registry = EndpointRegistry::empty();
        let activity = insert_with_reader(&mut registry, &sent, now);
        let snapshot_at = now + Duration::from_millis(1);
        activity.record(snapshot_at, true);
        let ping_at = snapshot_at + crate::limits::HEARTBEAT_INTERVAL;
        sent.lock().expect("test precondition").clear();
        registry.tick_health(ping_at);
        assert!(matches!(
            sent.lock().expect("test precondition").as_slice(),
            [ClientMessage::HealthPing]
        ));
        registry.received(&ssh_id, generation(2), ping_at + Duration::from_secs(1));
        registry.tick_health(ping_at + crate::limits::HEARTBEAT_TIMEOUT);
        assert!(registry.connection(&ssh_id).is_none());
        assert_eq!(registry.take_failures()[0].kind, io::ErrorKind::TimedOut);
    }

    #[test]
    fn next_service_deadline_tracks_reader_activity() {
        let mut registry = EndpointRegistry::empty();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let now = Instant::now();
        let activity = insert_with_reader(&mut registry, &sent, now);
        assert_eq!(
            registry.next_service_deadline(now),
            Some(now + crate::limits::HEARTBEAT_INTERVAL)
        );

        let received_at = now + Duration::from_millis(1);
        activity.record(received_at, true);
        assert_eq!(
            registry.next_service_deadline(now),
            Some(received_at + crate::limits::HEARTBEAT_INTERVAL)
        );
    }

    #[test]
    fn a_queued_failure_makes_the_service_deadline_due() {
        // A queued failure wakes the loop at once, ahead of the heartbeat deadline.
        let now = Instant::now();
        let mut registry = EndpointRegistry::new_synthetic_at(
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: Some(io::ErrorKind::BrokenPipe),
            },
            generation(1),
            now,
        );
        assert_eq!(
            registry.next_service_deadline(now),
            Some(now + crate::limits::HEARTBEAT_INTERVAL)
        );
        assert_eq!(
            registry.send_to(
                &ClientEndpointId::Local,
                &ClientMessage::ClientShellFocus { focused: true }
            ),
            EndpointSendOutcome::NotSent
        );
        assert_eq!(registry.next_service_deadline(now), Some(now));
        assert_eq!(registry.take_failures().len(), 1);
        assert_eq!(registry.next_service_deadline(now), None);
    }

    #[test]
    fn dropping_registry_detaches_every_connected_endpoint() {
        let local_sent = Arc::new(Mutex::new(Vec::new()));
        let remote_sent = Arc::new(Mutex::new(Vec::new()));
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::clone(&local_sent),
                error: None,
            },
            generation(1),
        );
        registry.insert(
            ClientEndpointId::Ssh(profile()),
            FakeTransport {
                sent: Arc::clone(&remote_sent),
                error: None,
            },
            generation(2),
            false,
            Instant::now(),
        );

        drop(registry);

        assert!(matches!(
            local_sent.lock().expect("test precondition").as_slice(),
            [ClientMessage::Detach]
        ));
        assert!(matches!(
            remote_sent.lock().expect("test precondition").as_slice(),
            [ClientMessage::Detach]
        ));
    }

    #[test]
    fn an_interactive_detach_is_not_sent_again_on_drop() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::clone(&sent),
                error: None,
            },
            generation(1),
        );

        assert_eq!(
            registry.send_to(&ClientEndpointId::Local, &ClientMessage::Detach),
            EndpointSendOutcome::Sent
        );
        assert_eq!(
            registry.send_to(&ClientEndpointId::Local, &ClientMessage::Detach),
            EndpointSendOutcome::Sent
        );
        drop(registry);

        assert!(matches!(
            sent.lock().expect("test precondition").as_slice(),
            [ClientMessage::Detach]
        ));
    }

    #[test]
    fn stale_generations_are_rejected() {
        let registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            generation(7),
        );
        assert!(registry.accepts(&ClientEndpointId::Local, generation(7)));
        assert!(!registry.accepts(&ClientEndpointId::Local, generation(6)));
    }
    #[test]
    fn send_viewed_reaches_only_viewed_connections() {
        let local = crate::tests::endpoints::RecordingTransport::default();
        let other = crate::tests::endpoints::RecordingTransport::default();
        let id = ClientEndpointId::Ssh(profile());
        let mut registry = EndpointRegistry::new(local.clone(), generation(1));
        registry.insert(
            id.clone(),
            other.clone(),
            generation(7),
            false,
            Instant::now(),
        );
        let msg = ClientMessage::ClientShellFocus { focused: true };
        registry.send_viewed(&msg);
        assert_eq!(local.take(), vec![msg.clone()]);
        assert!(other.take().is_empty());
        registry.set_viewed(&id, true);
        registry.send_viewed(&msg);
        assert_eq!(local.take(), vec![msg.clone()]);
        assert_eq!(other.take(), vec![msg]);
    }
    #[test]
    fn send_viewed_records_a_failed_connection_and_still_reaches_the_rest() {
        let local = crate::tests::endpoints::RecordingTransport::default();
        let other = crate::tests::endpoints::RecordingTransport::default();
        let id = ClientEndpointId::Ssh(profile());
        let mut registry = EndpointRegistry::new(local.clone(), generation(1));
        registry.insert(
            id.clone(),
            other.clone(),
            generation(7),
            true,
            Instant::now(),
        );
        other.fail_next();
        registry.send_viewed(&ClientMessage::ClientShellFocus { focused: true });
        assert_eq!(local.take().len(), 1);
        assert!(registry.connection(&id).is_none());
        assert_eq!(registry.take_failures()[0].endpoint_id, id);
    }
    #[test]
    fn release_unwanted_views_releases_every_resolvable_one_in_one_pass() {
        let mut registry = EndpointRegistry::empty();
        let boot = crate::tests::test_boot_id("boot");
        let transports: Vec<_> = (0..4)
            .map(|_| crate::tests::endpoints::RecordingTransport::default())
            .collect();
        let ids: Vec<_> = (0..4)
            .map(|n| {
                ClientEndpointId::Ssh(
                    crate::endpoint::MachineLabel::parse(format!("m{n}")).expect("machine"),
                )
            })
            .collect();
        for (id, transport) in ids.iter().zip(&transports) {
            registry.insert(
                id.clone(),
                transport.clone(),
                generation(7),
                true,
                Instant::now(),
            );
        }
        assert_eq!(
            registry
                .release_unwanted_views(|id| id == &ids[3], |id| (id != &ids[2]).then_some(&boot),),
            2
        );
        for index in 0..2 {
            assert!(!registry.viewed(&ids[index]));
            assert!(matches!(
                transports[index].take().as_slice(),
                [
                    ClientMessage::ClientShellFocus { focused: false },
                    ClientMessage::ClientShellEndpointRequest {
                        command: shepr_protocol::command::EndpointCommand::ClientShellSurfaceSet(
                            shepr_protocol::command::ClientShellSurfaceSetParams { active: false }
                        ),
                        ..
                    }
                ]
            ));
        }
        for index in 2..4 {
            assert!(registry.viewed(&ids[index]));
            assert!(transports[index].take().is_empty());
        }
    }
}
