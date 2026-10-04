use std::time::Instant;

/// Endpoint heartbeat interval, the connection-health cadence the remote
/// host's SSH bridge expiry is checked against.
pub(super) const HEARTBEAT_INTERVAL: std::time::Duration =
    shepr_launch::connection_health::HEARTBEAT_INTERVAL;
/// Expire an endpoint after this much transport silence, measured when the reader receives a
/// complete frame rather than when the client loop processes it.
///
/// The timeout allows ordinary network delay before marking a machine endpoint offline.
pub(super) const HEARTBEAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

const _: () = assert!(HEARTBEAT_INTERVAL.as_millis() < HEARTBEAT_TIMEOUT.as_millis());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HealthAction {
    None,
    Ping,
    Expired,
}

pub(super) struct EndpointHealth {
    connected_at: Instant,
    last_received: Instant,
    ping_sent_at: Option<Instant>,
    ready: bool,
}

impl EndpointHealth {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            connected_at: now,
            last_received: now,
            ping_sent_at: None,
            ready: false,
        }
    }

    pub(super) fn sync_reader_activity(
        &mut self,
        received_at: Option<Instant>,
        initial_snapshot_received: bool,
    ) {
        // A queued frame can be processed after a probe was sent; only a reader stamp after
        // the send marker proves activity in response to that probe.
        if let Some(received_at) = received_at
            && received_at > self.last_received
        {
            self.last_received = received_at;
            if self
                .ping_sent_at
                .is_none_or(|ping_sent_at| received_at > ping_sent_at)
            {
                self.ping_sent_at = None;
            }
        }
        if initial_snapshot_received {
            self.ready = true;
        }
    }

    pub(super) fn ready(&mut self) {
        self.ready = true;
    }

    pub(super) fn action(&self, now: Instant) -> HealthAction {
        let initial_snapshot_expired =
            !self.ready && now.saturating_duration_since(self.connected_at) >= HEARTBEAT_TIMEOUT;
        let probe_expired = self
            .ping_sent_at
            .is_some_and(|sent_at| now.saturating_duration_since(sent_at) >= HEARTBEAT_TIMEOUT);
        if initial_snapshot_expired || probe_expired {
            HealthAction::Expired
        } else if self.ping_sent_at.is_none()
            && now.saturating_duration_since(self.last_received) >= HEARTBEAT_INTERVAL
        {
            HealthAction::Ping
        } else {
            HealthAction::None
        }
    }

    pub(super) fn next_deadline(&self) -> Instant {
        let service_deadline = self.ping_sent_at.map_or_else(
            || self.last_received + HEARTBEAT_INTERVAL,
            |sent_at| sent_at + HEARTBEAT_TIMEOUT,
        );
        if self.ready {
            service_deadline
        } else {
            service_deadline.min(self.connected_at + HEARTBEAT_TIMEOUT)
        }
    }

    pub(super) fn ping_sent(&mut self, now: Instant) {
        self.ping_sent_at = Some(now);
    }
}

#[cfg(test)]
impl EndpointHealth {
    pub(super) fn received(&mut self, now: Instant) {
        self.last_received = now;
        self.ping_sent_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn quiet_connection_is_probed_then_expires_without_a_reply() {
        let now = Instant::now();
        let mut health = EndpointHealth::new(now);
        assert_eq!(health.action(now), HealthAction::None);
        assert_eq!(health.action(now + HEARTBEAT_INTERVAL), HealthAction::Ping);
        health.ping_sent(now + HEARTBEAT_INTERVAL);
        assert_eq!(
            health.action(now + HEARTBEAT_INTERVAL + HEARTBEAT_TIMEOUT),
            HealthAction::Expired
        );
    }

    #[test]
    fn any_incoming_message_satisfies_an_outstanding_probe() {
        let now = Instant::now();
        let mut health = EndpointHealth::new(now);
        health.ready();
        health.ping_sent(now);
        health.received(now + HEARTBEAT_TIMEOUT - Duration::from_millis(1));
        assert_eq!(health.action(now + HEARTBEAT_TIMEOUT), HealthAction::None);
    }

    #[test]
    fn heartbeats_do_not_hide_a_missing_initial_snapshot() {
        let now = Instant::now();
        let mut health = EndpointHealth::new(now);
        health.ping_sent(now + HEARTBEAT_INTERVAL);
        health.received(now + HEARTBEAT_INTERVAL + Duration::from_secs(1));
        assert_eq!(
            health.action(now + HEARTBEAT_TIMEOUT),
            HealthAction::Expired
        );
    }

    #[test]
    fn next_deadline_covers_pings_probes_and_missing_snapshots() {
        let now = Instant::now();
        let mut health = EndpointHealth::new(now);
        assert_eq!(health.next_deadline(), now + HEARTBEAT_INTERVAL);

        health.ping_sent(now + HEARTBEAT_INTERVAL);
        assert_eq!(health.next_deadline(), now + HEARTBEAT_TIMEOUT);

        health.ready();
        let ping_sent_at = now + HEARTBEAT_INTERVAL + Duration::from_secs(1);
        health.ping_sent(ping_sent_at);
        assert_eq!(health.next_deadline(), ping_sent_at + HEARTBEAT_TIMEOUT);

        let received_at = ping_sent_at + Duration::from_secs(1);
        health.received(received_at);
        assert_eq!(health.next_deadline(), received_at + HEARTBEAT_INTERVAL);
    }

    #[test]
    fn a_reader_frame_from_before_the_probe_does_not_answer_it() {
        let now = Instant::now();
        let mut health = EndpointHealth::new(now);
        health.ready();
        let received_at = now + Duration::from_millis(1);
        let ping_sent_at = now + Duration::from_millis(2);
        health.ping_sent(ping_sent_at);
        health.sync_reader_activity(Some(received_at), false);
        assert_eq!(
            health.action(ping_sent_at + HEARTBEAT_TIMEOUT),
            HealthAction::Expired
        );

        let response_at = ping_sent_at + Duration::from_millis(1);
        health.sync_reader_activity(Some(response_at), false);
        assert_eq!(
            health.action(response_at + HEARTBEAT_INTERVAL - Duration::from_millis(1)),
            HealthAction::None
        );
    }
}
