use std::collections::HashMap;
use std::io;
use std::time::Instant;

use super::ClientEndpointId;
use super::health::{EndpointHealth, HealthAction};
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

/// What the Local slot's socket leads to, which decides whether it needs heartbeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalEndpointLink {
    /// A server socket on this host. A dead server shows up as a transport error, so
    /// no heartbeat is needed.
    Socket,
    /// The `shepr --remote` bridge socket: every byte crosses SSH, like a saved machine.
    /// The link can go silent without an error, and the remote end of the bridge exits
    /// after an idle stretch unless heartbeat traffic keeps it busy.
    SshBridge,
}

pub struct EndpointRegistry {
    active: ClientEndpointId,
    input_enabled: bool,
    local_link: LocalEndpointLink,
    connections: HashMap<ClientEndpointId, EndpointConnection>,
    failures: Vec<EndpointTransportFailure>,
}

impl EndpointRegistry {
    pub(crate) fn empty(local_link: LocalEndpointLink) -> Self {
        Self {
            active: ClientEndpointId::Local,
            input_enabled: false,
            local_link,
            connections: HashMap::new(),
            failures: Vec::new(),
        }
    }

    /// A registry whose Local slot is a server socket on this host, connected at `now`.
    pub fn new_at(local: impl EndpointTransport + 'static, generation: u64, now: Instant) -> Self {
        Self::with_local_link(local, generation, LocalEndpointLink::Socket, now)
    }

    pub(crate) fn with_local_link(
        local: impl EndpointTransport + 'static,
        generation: u64,
        local_link: LocalEndpointLink,
        now: Instant,
    ) -> Self {
        let mut registry = Self::empty(local_link);
        registry.input_enabled = true;
        registry.insert(ClientEndpointId::Local, local, generation, true, now);
        registry
    }

    /// Every connection that crosses SSH gets heartbeats and a silence deadline: saved
    /// machines always, and the Local slot when it is the `--remote` bridge.
    fn crosses_ssh(&self, endpoint_id: &ClientEndpointId) -> bool {
        match endpoint_id {
            ClientEndpointId::Local => self.local_link == LocalEndpointLink::SshBridge,
            ClientEndpointId::Ssh(_) => true,
        }
    }

    pub fn active_id(&self) -> &ClientEndpointId {
        &self.active
    }

    pub fn active_surface_available(&self) -> bool {
        self.input_enabled
            && self
                .connections
                .get(&self.active)
                .is_some_and(|connection| connection.surface_active)
    }

    pub(crate) fn freeze_input(&mut self) {
        self.input_enabled = false;
    }

    pub fn unfreeze_input(&mut self) {
        self.input_enabled = true;
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
        let health = self
            .crosses_ssh(&endpoint_id)
            .then(|| EndpointHealth::new(now));
        if let Some(mut previous) = self.connections.insert(
            endpoint_id,
            EndpointConnection {
                transport: Box::new(transport),
                generation: generation.into(),
                surface_active,
                health,
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
        if let Some(health) = self
            .connections
            .get_mut(endpoint_id)
            .filter(|connection| connection.generation == generation)
            .and_then(|connection| connection.health.as_mut())
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
            .iter()
            .filter_map(|(endpoint_id, connection)| {
                connection
                    .health
                    .as_ref()
                    .map(|health| (endpoint_id.clone(), health.action(now)))
            })
            .filter(|(_, action)| *action != HealthAction::None)
            .collect::<Vec<_>>();
        for (endpoint_id, action) in actions {
            match action {
                HealthAction::None => {}
                HealthAction::Ping => {
                    let ping = ClientMessage::HealthPing(String::new());
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

    pub(crate) fn flush_active(&mut self, deadline: Instant) -> EndpointSendOutcome {
        let endpoint_id = self.active.clone();
        let result = self
            .connections
            .get_mut(&endpoint_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "endpoint is unavailable"))
            .and_then(|connection| connection.transport.flush(deadline));
        match result {
            Ok(()) => EndpointSendOutcome::Sent,
            Err(error) => {
                self.record_failure(&endpoint_id, &error);
                EndpointSendOutcome::NotSent
            }
        }
    }

    pub(crate) fn send_to(
        &mut self,
        endpoint_id: &ClientEndpointId,
        message: &ClientMessage,
    ) -> EndpointSendOutcome {
        let result = self
            .connections
            .get_mut(endpoint_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "endpoint is unavailable"))
            .and_then(|connection| connection.transport.send(message));
        match result {
            Ok(()) => EndpointSendOutcome::Sent,
            Err(error) => {
                self.record_failure(endpoint_id, &error);
                EndpointSendOutcome::NotSent
            }
        }
    }

    pub(crate) fn disconnect(&mut self, endpoint_id: &ClientEndpointId) {
        self.failures
            .retain(|failure| &failure.endpoint_id != endpoint_id);
        if let Some(mut connection) = self.connections.remove(endpoint_id) {
            connection.transport.disconnect();
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
        let Some(generation) = self
            .connections
            .get(endpoint_id)
            .map(|connection| connection.generation)
        else {
            return;
        };
        let failure = EndpointTransportFailure {
            endpoint_id: endpoint_id.clone(),
            generation: generation.get(),
            kind: error.kind(),
            message: error.to_string(),
        };
        if let Some(mut connection) = self.connections.remove(endpoint_id) {
            connection.transport.disconnect();
        }
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
        // Detach is a courtesy on the way out: every connection is disconnected just
        // below, and a server treats the closed connection as this client leaving, so a
        // Detach that fails to send or flush changes nothing.
        for connection in self.connections.values_mut() {
            connection.transport.send(&ClientMessage::Detach).ok();
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
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

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

    fn profile() -> crate::endpoint::ProfileId {
        crate::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
            .expect("test precondition")
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
                crate::endpoint::ProfileId::parse(format!("{index:032x}"))
                    .expect("test profile id"),
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
        let mut registry = EndpointRegistry::empty(LocalEndpointLink::Socket);
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
        let mut registry = EndpointRegistry::empty(LocalEndpointLink::Socket);
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
    fn only_a_local_slot_behind_the_ssh_bridge_is_health_tracked() {
        let socket_sent = Arc::new(Mutex::new(Vec::new()));
        let mut socket = EndpointRegistry::new(
            FakeTransport {
                sent: Arc::clone(&socket_sent),
                error: None,
            },
            1,
        );
        assert!(
            socket
                .connection(&ClientEndpointId::Local)
                .is_some_and(|connection| connection.health.is_none())
        );

        let bridge_sent = Arc::new(Mutex::new(Vec::new()));
        let mut bridge = EndpointRegistry::with_local_link(
            FakeTransport {
                sent: Arc::clone(&bridge_sent),
                error: None,
            },
            1,
            LocalEndpointLink::SshBridge,
            Instant::now(),
        );
        // Taken after both inserts, so each connection's health clock started earlier.
        let now = Instant::now();
        let ping_at = now + crate::limits::HEARTBEAT_INTERVAL;
        let expire_at = ping_at + crate::limits::HEARTBEAT_TIMEOUT;

        socket.tick_health(ping_at);
        socket.tick_health(expire_at);
        assert!(socket_sent.lock().expect("test precondition").is_empty());
        assert!(socket.connection(&ClientEndpointId::Local).is_some());
        assert!(socket.take_failures().is_empty());

        assert!(
            bridge
                .connection(&ClientEndpointId::Local)
                .is_some_and(|connection| connection.health.is_some())
        );
        bridge.mark_ready(&ClientEndpointId::Local, 1);
        bridge.tick_health(ping_at);
        assert!(matches!(
            bridge_sent.lock().expect("test precondition").as_slice(),
            [ClientMessage::HealthPing(_)]
        ));
        bridge.tick_health(expire_at);
        assert!(bridge.connection(&ClientEndpointId::Local).is_none());
        let failures = bridge.take_failures();
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
            [ClientMessage::HealthPing(_)]
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
