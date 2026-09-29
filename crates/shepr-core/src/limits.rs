//! Shared timing, geometry, and layout limits owned by the core crate.

use std::time::Duration;

/// An SSH bridge must outlive several client heartbeat cycles while idle.
/// The one-minute window gives a healthy bridge multiple chances to answer
/// five-second endpoint probes; its minimum ratio is checked below.
pub const BRIDGE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// A connected client probes an endpoint after this much silence.
/// Five seconds gives routine SSH and server scheduling room between probes.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// One cold SSH round trip, including a noninteractive command or status probe.
/// Fifteen seconds bounds a slow startup without letting a hung host block the
/// caller.
pub const SSH_ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(15);

/// Minimum number of client heartbeat intervals that a quiet bridge survives.
/// Three cycles allow multiple delayed probes before the bridge is considered
/// idle.
pub(crate) const BRIDGE_IDLE_MIN_HEARTBEAT_CYCLES: u32 = 3;

const _: () = assert!(
    BRIDGE_IDLE_TIMEOUT.as_millis()
        >= HEARTBEAT_INTERVAL
            .saturating_mul(BRIDGE_IDLE_MIN_HEARTBEAT_CYCLES)
            .as_millis()
);

/// Total share represented by both children of a normalized split; 1.0 is the
/// full layout area.
pub(crate) const SPLIT_RATIO_TOTAL: f32 = 1.0;
/// Smallest permitted first-child share; ten percent keeps an asymmetric split
/// from collapsing either pane.
pub const MIN_SPLIT_RATIO: f32 = 0.1;
/// Largest permitted first-child share, leaving the minimum share to the
/// second pane.
pub const MAX_SPLIT_RATIO: f32 = SPLIT_RATIO_TOTAL - MIN_SPLIT_RATIO;
/// First-child share used when a split has no explicit ratio; half gives both
/// children equal space.
pub const EVEN_SPLIT: f32 = 0.5;

/// Smallest pane grid width in columns; narrower panes leave too little room
/// for terminal text.
pub(crate) const PANE_MIN_COLS: u16 = 4;
/// Smallest pane grid height in rows.
pub(crate) const PANE_MIN_ROWS: u16 = 2;

/// First ID available to a real pane; the reserved placeholder ID is excluded.
pub(crate) const FIRST_PANE_ID: u32 = 1;
/// Reserved ID used while a layout operation temporarily removes a pane.
pub(crate) const PLACEHOLDER_PANE_ID: u32 = 0;

/// Divider distance in cells accepted when selecting a split for keyboard
/// resize. One cell absorbs integer-coordinate edge rounding.
pub(crate) const SPLIT_EDGE_MATCH_TOLERANCE_CELLS: u32 = 1;
/// Minimum number of cells assigned to each child when a split has room for
/// both. One cell keeps each child representable.
pub(crate) const MIN_SPLIT_CHILD_CELLS: u16 = 1;
/// Axis size needed to give both children their minimum extent, derived from
/// the one-cell minimum for each child.
pub(crate) const MIN_SPLIT_EXTENT_CELLS: u16 = MIN_SPLIT_CHILD_CELLS + MIN_SPLIT_CHILD_CELLS;
/// A workspace keeps at least one pane when removing or moving panes.
pub(crate) const MIN_WORKSPACE_PANES: usize = 1;
