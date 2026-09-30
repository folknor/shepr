use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Instant;

use super::ClientEndpointId;
use super::health::{EndpointHealth, HealthAction};
use super::writer::{EndpointReadActivity, NativeEndpointTransport};
use crate::limits::ENDPOINT_DETACH_FLUSH_TIMEOUT;
use shepr_protocol::ClientMessage;

pub trait EndpointTransport: Send {
    fn send(&mut self, message: &ClientMessage) -> io::Result<()>;

    fn disconnect(&mut self);

    fn flush(&mut self, deadline: Instant) -> io::Result<()>;

    fn take_error(&mut self) -> Option<io::Error>;
}

pub(crate) struct EndpointConnection {
    transport: Box<dyn EndpointTransport>,
    pub(crate) generation: shepr_protocol::ConnectionGeneration,
    pub(crate) surface_active: bool,
    health: Option<EndpointHealth>,
    /// Frame arrivals as the reader thread stamps them; a connection that has one takes
    /// its health from it rather than from the client loop's processing.
    read_activity: Option<Arc<EndpointReadActivity>>,
    // Explicit detach paths use send_to before the registry is dropped.
    detach_sent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EndpointTransportFailure {
    pub(crate) endpoint_id: ClientEndpointId,
    pub(crate) generation: u64,
    pub(crate) kind: io::ErrorKind,
    pub(crate) message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndpointSendOutcome {
    Sent,
    NotSent,
}

pub struct EndpointRegistry {
    active: ClientEndpointId,
    connections: HashMap<ClientEndpointId, EndpointConnection>,
    failures: Vec<EndpointTransportFailure>,
}

impl EndpointRegistry {
    pub(crate) fn empty() -> Self {
        Self {
            active: ClientEndpointId::Local,
            connections: HashMap::new(),
            failures: Vec::new(),
        }
    }

    /// A registry whose Local slot is a server socket on this host, connected at `now`.
    pub fn new_at(local: impl EndpointTransport + 'static, generation: u64, now: Instant) -> Self {
        let mut registry = Self::empty();
        registry.insert(ClientEndpointId::Local, local, generation, true, now);
        registry
    }

    /// The reader timestamps complete frames on machine connections before it queues them for
    /// the client loop. Health deadlines therefore measure transport silence, not time spent
    /// waiting for the client loop to process its event queue. Local uses a socket on this host
    /// and reports a dead server as a transport error, so it needs no heartbeat.
    fn crosses_ssh(endpoint_id: &ClientEndpointId) -> bool {
        matches!(endpoint_id, ClientEndpointId::Ssh(_))
    }

    pub fn active_id(&self) -> &ClientEndpointId {
        &self.active
    }

    /// Whether the active endpoint's connection holds a live surface. This is transport state
    /// only: who owns the presentation, and so whether pane input may flow, is the client's
    /// `Presentation`, which also requires `Owned` (see
    /// `shell_runtime::active_endpoint_owns_presentation`).
    pub fn active_surface_available(&self) -> bool {
        self.connections
            .get(&self.active)
            .is_some_and(|connection| connection.surface_active)
    }

    pub(crate) fn connection(&self, endpoint_id: &ClientEndpointId) -> Option<&EndpointConnection> {
        self.connections.get(endpoint_id)
    }

    pub fn insert(
        &mut self,
        endpoint_id: ClientEndpointId,
        transport: impl EndpointTransport + 'static,
        generation: u64,
        surface_active: bool,
        now: Instant,
    ) {
        self.insert_with_activity(
            endpoint_id,
            transport,
            generation,
            surface_active,
            None,
            now,
        );
    }

    pub(crate) fn insert_native(
        &mut self,
        endpoint_id: ClientEndpointId,
        transport: NativeEndpointTransport,
        generation: u64,
        surface_active: bool,
        now: Instant,
    ) {
        let read_activity = Some(transport.read_activity());
        self.insert_with_activity(
            endpoint_id,
            transport,
            generation,
            surface_active,
            read_activity,
            now,
        );
    }

    fn insert_with_activity(
        &mut self,
        endpoint_id: ClientEndpointId,
        transport: impl EndpointTransport + 'static,
        generation: u64,
        surface_active: bool,
        read_activity: Option<Arc<EndpointReadActivity>>,
        now: Instant,
    ) {
        let health = Self::crosses_ssh(&endpoint_id).then(|| EndpointHealth::new(now));
        if let Some(mut previous) = self.connections.insert(
            endpoint_id,
            EndpointConnection {
                transport: Box::new(transport),
                generation: generation.into(),
                surface_active,
                health,
                read_activity,
                detach_sent: false,
            },
        ) {
            previous.transport.disconnect();
        }
    }

    pub(crate) fn accepts(&self, endpoint_id: &ClientEndpointId, generation: u64) -> bool {
        self.connections
            .get(endpoint_id)
            .is_some_and(|connection| connection.generation == generation)
    }

    pub(crate) fn received(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        now: Instant,
    ) {
        if let Some(connection) = self
            .connections
            .get_mut(endpoint_id)
            .filter(|connection| connection.generation == generation)
            .filter(|connection| connection.read_activity.is_none())
            && let Some(health) = connection.health.as_mut()
        {
            health.received(now);
        }
    }

    pub(crate) fn mark_ready(&mut self, endpoint_id: &ClientEndpointId, generation: u64) {
        if let Some(health) = self
            .connections
            .get_mut(endpoint_id)
            .filter(|connection| connection.generation == generation)
            .and_then(|connection| connection.health.as_mut())
        {
            health.ready();
        }
    }

    pub(crate) fn tick_health(&mut self, now: Instant) {
        let actions = self
            .connections
            .iter_mut()
            .filter_map(|(endpoint_id, connection)| {
                let health = connection.health.as_mut()?;
                if let Some(read_activity) = &connection.read_activity {
                    let (received_at, snapshot_received) = read_activity.observed();
                    health.sync_reader_activity(received_at, snapshot_received);
                }
                Some((endpoint_id.clone(), health.action(now)))
            })
            .filter(|(_, action)| *action != HealthAction::None)
            .collect::<Vec<_>>();
        for (endpoint_id, action) in actions {
            match action {
                HealthAction::None => {}
                HealthAction::Ping => {
                    let ping = ClientMessage::HealthPing;
                    if self.send_to(&endpoint_id, &ping) == EndpointSendOutcome::Sent
                        && let Some(health) = self
                            .connections
                            .get_mut(&endpoint_id)
                            .and_then(|connection| connection.health.as_mut())
                    {
                        health.ping_sent(now);
                    }
                }
                HealthAction::Expired => self.record_failure(
                    &endpoint_id,
                    &io::Error::new(io::ErrorKind::TimedOut, "endpoint health check timed out"),
                ),
            }
        }
    }

    pub(crate) fn set_active(&mut self, endpoint_id: &ClientEndpointId) -> bool {
        if !self
            .connections
            .get(endpoint_id)
            .is_some_and(|connection| connection.surface_active)
        {
            return false;
        }
        self.active = endpoint_id.clone();
        true
    }

    pub(crate) fn set_surface_active(
        &mut self,
        endpoint_id: &ClientEndpointId,
        active: bool,
    ) -> bool {
        let Some(connection) = self.connections.get_mut(endpoint_id) else {
            return false;
        };
        let changed = connection.surface_active != active;
        connection.surface_active = active;
        changed
    }

    pub(crate) fn send(&mut self, message: &ClientMessage) -> EndpointSendOutcome {
        let endpoint_id = self.active.clone();
        self.send_to(&endpoint_id, message)
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
            generation: connection.generation.get(),
            kind: error.kind(),
            message: error.to_string(),
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
        let deadline =
            crate::limits::Deadline::after(Instant::now(), ENDPOINT_DETACH_FLUSH_TIMEOUT);
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
    pub fn new(local: impl EndpointTransport + 'static, generation: u64) -> Self {
        // clock-io-ok: this test-only constructor stands in for the client launch.
        Self::new_at(local, generation, Instant::now())
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
    fn endpoint_failures_do_not_remove_other_connections() {
        let local_sent = Arc::new(Mutex::new(Vec::new()));
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::clone(&local_sent),
                error: None,
            },
            1,
        );
        let ssh_id = ClientEndpointId::Ssh(profile());
        registry.insert(
            ssh_id.clone(),
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: Some(io::ErrorKind::BrokenPipe),
            },
            2,
            true,
            Instant::now(),
        );
        assert!(registry.set_active(&ssh_id));

        assert_eq!(
            registry.send(&ClientMessage::ClientShellFocus { focused: true }),
            EndpointSendOutcome::NotSent
        );
        assert!(registry.connection(&ssh_id).is_none());
        assert!(registry.connection(&ClientEndpointId::Local).is_some());
        assert_eq!(registry.take_failures()[0].endpoint_id, ssh_id);

        assert!(registry.set_active(&ClientEndpointId::Local));
        assert_eq!(
            registry.send(&ClientMessage::ClientShellFocus { focused: true }),
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
        fn insert(registry: &mut EndpointRegistry, index: usize, generation: u64) {
            registry.insert(
                endpoint_id(index),
                FakeTransport {
                    sent: Arc::new(Mutex::new(Vec::new())),
                    error: None,
                },
                generation,
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
        assert_eq!((first.generation, first.kind), (2, io::ErrorKind::TimedOut));
    }

    #[test]
    fn reconnecting_active_identity_does_not_count_as_an_active_surface() {
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            1,
        );
        let ssh_id = ClientEndpointId::Ssh(profile());
        registry.insert(
            ssh_id.clone(),
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            2,
            true,
            Instant::now(),
        );
        assert!(registry.set_active(&ssh_id));
        assert!(registry.active_surface_available());
        registry.disconnect(&ssh_id);
        registry.insert(
            ssh_id,
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            3,
            false,
            Instant::now(),
        );
        assert!(!registry.active_surface_available());
    }

    #[test]
    fn recovered_local_uses_transport_failure_not_remote_health_probes() {
        let mut registry = EndpointRegistry::empty();
        let sent = Arc::new(Mutex::new(Vec::new()));
        registry.insert(
            ClientEndpointId::Local,
            FakeTransport {
                sent: Arc::clone(&sent),
                error: None,
            },
            2,
            false,
            Instant::now(),
        );
        registry.tick_health(Instant::now() + std::time::Duration::from_secs(300));
        assert!(registry.connection(&ClientEndpointId::Local).is_some());
        assert!(sent.lock().expect("test precondition").is_empty());
        assert!(registry.take_failures().is_empty());
    }

    #[test]
    fn a_local_slot_is_not_health_tracked() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut socket = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::clone(&sent),
                error: None,
            },
            1,
        );
        assert!(
            socket
                .connection(&ClientEndpointId::Local)
                .is_some_and(|connection| connection.health.is_none())
        );

        // Taken after the insert, so the connection's health clock started earlier.
        let now = Instant::now();
        let ping_at = now + crate::limits::HEARTBEAT_INTERVAL;
        let expire_at = ping_at + crate::limits::HEARTBEAT_TIMEOUT;

        socket.tick_health(ping_at);
        socket.tick_health(expire_at);
        assert!(sent.lock().expect("test precondition").is_empty());
        assert!(socket.connection(&ClientEndpointId::Local).is_some());
        assert!(socket.take_failures().is_empty());
    }

    #[test]
    fn negotiated_remote_health_probe_expires_the_connection() {
        let mut registry = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            1,
        );
        let ssh_id = ClientEndpointId::Ssh(profile());
        let sent = Arc::new(Mutex::new(Vec::new()));
        registry.insert(
            ssh_id.clone(),
            FakeTransport {
                sent: Arc::clone(&sent),
                error: None,
            },
            2,
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
            1,
        );
        let ssh_id = ClientEndpointId::Ssh(profile());
        registry.insert(
            ssh_id.clone(),
            FakeTransport {
                sent: Arc::new(Mutex::new(Vec::new())),
                error: None,
            },
            2,
            false,
            Instant::now(),
        );
        let now = Instant::now();
        registry.mark_ready(&ssh_id, 2);
        registry.received(&ssh_id, 2, now + crate::limits::HEARTBEAT_INTERVAL);
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
            2,
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
        registry.received(&ssh_id, 2, ping_at + Duration::from_secs(1));
        registry.tick_health(ping_at + crate::limits::HEARTBEAT_TIMEOUT);
        assert!(registry.connection(&ssh_id).is_none());
        assert_eq!(registry.take_failures()[0].kind, io::ErrorKind::TimedOut);
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
            1,
        );
        registry.insert(
            ClientEndpointId::Ssh(profile()),
            FakeTransport {
                sent: Arc::clone(&remote_sent),
                error: None,
            },
            2,
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
            1,
        );

        assert_eq!(
            registry.send(&ClientMessage::Detach),
            EndpointSendOutcome::Sent
        );
        assert_eq!(
            registry.send(&ClientMessage::Detach),
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
            7,
        );
        assert!(registry.accepts(&ClientEndpointId::Local, 7));
        assert!(!registry.accepts(&ClientEndpointId::Local, 6));
    }
}
