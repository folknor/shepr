//! Timing limits shared by crates on opposite sides of a dependency boundary.

use std::time::Duration;

/// An SSH bridge must outlive several client heartbeat cycles while idle.
pub const BRIDGE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// A connected client probes an endpoint after this much silence.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// One cold SSH round trip, including a noninteractive command or status probe.
pub const SSH_ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(15);

const _: () =
    assert!(BRIDGE_IDLE_TIMEOUT.as_millis() >= HEARTBEAT_INTERVAL.saturating_mul(3).as_millis());
