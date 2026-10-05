//! The counters a pane's terminal core keeps. Each is an equality or ordering
//! token for one kind of change, so each is its own type: none can be passed
//! where another is meant, and the rules that read them (the parity of a
//! surface revision, the idle or held state of synchronized output) live here
//! instead of as arithmetic on a bare `u64` at the call sites. Only
//! `PaneTerminalCore::record_mutation` advances them.

/// The pane content revision is the wire's own type, which owns the parity
/// rule: the terminal core advances it on every render-visible mutation, under
/// the core lock, and a full surface read across several core holds is
/// certified by [`ContentRevision::certify`].
pub use shepr_protocol::ContentRevision;

/// Advances when screen detection's input may have changed: live output,
/// completed synchronized updates, clears and resizes only. Viewport and host
/// presentation changes do not invalidate screen scans.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DetectionSeq(u64);

impl DetectionSeq {
    pub(super) fn bump(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }

    /// The raw sequence, for the detector's equality tokens.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// An equality token paired with the synchronized-output flag under one core
/// hold: it moves whenever the flag flips or an update is flushed, so equal
/// idle epochs on either side of a draw mean no update began or ended in
/// between.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncEpoch(u64);

impl SyncEpoch {
    pub(super) fn advance(&mut self, steps: u8) {
        self.0 = self.0.wrapping_add(u64::from(steps));
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// What a pane's synchronized-output state allows a reader to do, read in one
/// core hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    /// No update is open; the epoch names this state.
    Idle(SyncEpoch),
    /// An update is open: the screen is not drawable until it ends.
    Active,
    /// The core lock is poisoned. The PTY actor closes the pane shortly, so
    /// readers defer rather than invent a state.
    Poisoned,
}

impl SyncState {
    /// Whether nothing can be drawn now: an open update or an unreadable core.
    pub fn holds_surface(self) -> bool {
        !matches!(self, Self::Idle(_))
    }
}

/// Advances every time the child sets a default colour (OSC 10/11), so an
/// owner probe made after the lock was released is recorded only if that
/// override is still the current one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DefaultColorGeneration(u64);

impl DefaultColorGeneration {
    pub(super) fn advance(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }
}
