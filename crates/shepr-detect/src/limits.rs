use std::time::Duration;

/// Maximum rules in one manifest. This sits well past the size of the bundled
/// manifests while keeping compilation and per-screen evaluation bounded.
pub(crate) const MAX_RULES_PER_MANIFEST: usize = 128;

/// Maximum nested gate depth, including the root gate. The bound covers the
/// bundled gate shapes while bounding recursive validation and evaluation.
pub(crate) const MAX_GATE_DEPTH: usize = 8;

/// Maximum gates in one manifest. At the per-manifest rule ceiling this permits
/// several gates per rule on average while bounding recursive compilation and
/// matching across all rules.
pub(crate) const MAX_TOTAL_GATES: usize = 512;

/// Maximum direct matchers on a gate. The fixed match-state array uses this
/// value, keeping gate evaluation allocation-free and its stack use bounded.
pub(crate) const MAX_MATCHERS_PER_GATE: usize = 32;

/// Maximum distinct regions in one manifest. Detection stores each extracted
/// region in a fixed array, so this bounds both cache size and extraction work.
pub(crate) const MAX_REGIONS_PER_MANIFEST: usize = 32;

/// Maximum matchers across a manifest. This permits several matchers per rule at
/// the rule ceiling while bounding regex compilation and each screen sample's
/// matching work.
pub(crate) const MAX_TOTAL_MATCHERS: usize = 1024;

/// Maximum characters in one matcher. The bound allows detailed screen
/// patterns while preventing oversized expressions from driving unbounded
/// compile work.
pub(crate) const MAX_MATCHER_CHARS: usize = 512;

/// Maximum characters retained in a manifest evidence preview. A few readable
/// lines preserve enough evidence to explain a detection without flooding output.
pub(crate) const MAX_MANIFEST_PREVIEW_CHARS: usize = 240;

/// Smallest accepted line count for a counted manifest region. Counted regions
/// must select a line to have a useful matching scope.
pub(crate) const MIN_REGION_LINE_COUNT: usize = 1;

/// Largest accepted line count for a counted manifest region. The schema caps
/// the decimal field at the largest value representable by its 16-bit range.
pub(crate) const MAX_REGION_LINE_COUNT: usize = u16::MAX as usize;

/// Hook reports are ordered per source by the `seq` each hook process takes
/// from its own wall clock (nanoseconds for the shell/python hooks,
/// microseconds for the JS plugins; only ever compared within one source).
/// A report whose `seq` is not above the last accepted one is normally a
/// straggler from a racing hook process and is dropped, however late it
/// arrives: silence is not evidence of anything. When the host's wall clock
/// has fallen this far behind its monotonic clock since the last acceptance,
/// the clock stepped backwards (NTP, a manual change), and dropping would lose
/// every report until it caught up again. Such a report is accepted and
/// re-anchors the source's sequence. A wall clock that reads earlier than it
/// did at the last acceptance is a backward step of any size and is accepted
/// the same way; this threshold covers a step the clock has since caught up
/// on.
pub(crate) const HOOK_SEQUENCE_REANCHOR_AFTER: Duration = Duration::from_secs(5);
/// How long a parked hook start stays available to attribute a process.
///
/// The mux rechecks an unidentified process on a fixed interval, including
/// when the foreground group remains unchanged, and its limits assert that
/// this attribution window spans several such intervals: room for scheduling
/// delay without keeping an identity available indefinitely for an
/// unrelated future process. No PID is supplied by either input, so this bounds
/// temporal attribution rather than proving identity. Both ends are monotonic
/// `Instant`s, so a wall-clock step cannot expire or extend a start.
pub const PARKED_START_LIFETIME: Duration = Duration::from_secs(120);
/// Maximum stale lifecycle sessions remembered per hook source, bounding
/// deduplication memory while retaining recent reports.
pub(crate) const MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE: usize = 64;

/// How close a pane's signal death (or a signal shutdown, on either side) must
/// follow an agent's exit for the resume identity that exit released to be
/// saved anyway: long enough for a group kill to reach the shell after the
/// agent, short enough that an unrelated shell death rarely revives an agent
/// the user quit.
pub(crate) const AGENT_PROCESS_EXIT_RELEASE_GRACE: Duration = Duration::from_millis(750);
