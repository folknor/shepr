//! Runtime budgets and retention limits for mux-owned state.

use std::time::Duration;

// Detection tunables: this section owns process probes, tick cadences and
// publication holds. crates/shepr-detect/src/limits.rs owns hook arbitration
// windows and manifest complexity bounds; the bundled manifests own each agent's sampled
// regions (top/bottom line depths), because those depend on its screen layout.
// The parked-start window is coupled to this cadence by the assertion below.

// Process probe scheduling: when the detector looks at the pane's process tree.

/// Consecutive process misses required before dropping an identified agent;
/// transient /proc gaps must not erase its state.
pub(crate) const AGENT_MISS_CONFIRMATION_ATTEMPTS: u8 = 6;
/// Recheck cadence for an already identified process, limiting probe work.
pub(crate) const PROCESS_RECHECK_IDENTIFIED: Duration = Duration::from_secs(5);
/// Recheck cadence for an unidentified process after acquisition settles.
/// This allows late-starting agents to be found without frequent /proc scans.
pub(crate) const PROCESS_RECHECK_UNIDENTIFIED: Duration = Duration::from_secs(30);
// A parked agent start must outlive several unidentified rechecks, so a
// process the acquisition window missed is still attributed by a later probe.
const _: () = assert!(
    shepr_detect::PARKED_START_LIFETIME.as_secs() >= 4 * PROCESS_RECHECK_UNIDENTIFIED.as_secs()
);

/// Total fast-to-slow acquisition window after a pane starts.
pub(crate) const PROCESS_ACQUISITION_WINDOW: Duration = Duration::from_secs(8);
/// Initial fast portion of the acquisition window, when agents are most likely
/// to appear after a shell launch.
pub(crate) const PROCESS_ACQUISITION_FAST_WINDOW: Duration = Duration::from_millis(1500);
/// Poll cadence within the fast acquisition window.
pub(crate) const PROCESS_ACQUISITION_FAST_RECHECK: Duration = Duration::from_millis(500);
/// Poll cadence after fast acquisition; the window still gets several attempts
/// without continuous /proc work.
pub(crate) const PROCESS_ACQUISITION_SLOW_RECHECK: Duration = Duration::from_secs(2);
/// Idle time before restarting acquisition after process activity subsides.
pub(crate) const PROCESS_ACQUISITION_IDLE_RESET: Duration = Duration::from_secs(2);

// The detector's tick cadence, chosen per tick from what the pane shows.

/// Probe cadence during a transient color override, when a visible state
/// change is expected immediately.
pub(crate) const PROCESS_RECHECK_TRANSIENT: Duration = Duration::from_millis(50);
/// How long after a foreground change the transient cadence runs. A
/// color-setting program that stays in the foreground or on the alternate
/// screen falls back to the ordinary cadence after this.
pub(crate) const TRANSIENT_COLOR_RECHECK_WINDOW: Duration = Duration::from_secs(2);
/// Probe cadence when no agent is identified, also used for the initial
/// scheduled poll, balancing acquisition latency against repeated scans.
pub(crate) const PROCESS_RECHECK_NO_AGENT: Duration = Duration::from_millis(500);
/// Probe cadence while tracking an agent whose visible state can change.
pub(crate) const PROCESS_RECHECK_ACTIVE_AGENT: Duration = Duration::from_millis(300);

// Agent state publication: confirming idle and holding through startup.

/// Recheck cadence while visible output suggests a pending idle transition.
pub(crate) const AGENT_PENDING_IDLE_RECHECK: Duration = Duration::from_millis(100);
/// Matching idle observations needed before publishing idle, filtering a
/// single transient frame.
pub(crate) const AGENT_PENDING_IDLE_CONFIRMATIONS: u8 = 3;
/// Longest time to hold a pending idle transition before publishing it.
pub(crate) const AGENT_PENDING_IDLE_CAP: Duration = Duration::from_millis(700);
/// Refresh cadence for a stable visible signal, avoiding a stale detection
/// result without polling every frame.
pub(crate) const STABLE_VISIBLE_SIGNAL_REFRESH: Duration = Duration::from_millis(800);
/// Startup grace for the first agent signal while a launched shell settles.
pub(crate) const AGENT_STARTUP_GRACE_WINDOW: Duration = Duration::from_secs(3);
/// Time allowed after an AgentResume launch for an agent to appear before
/// detection clears its seed. Fresh and ordinary restored shells have no hold.
pub(crate) const AGENT_ABSENCE_STARTUP_HOLD: Duration = Duration::from_secs(30);

// The pane terminal: detection reads, render pacing and scrollback scans.

/// Minimum history rows searched during resize recovery of a blank viewport.
/// Agent detection samples the live terminal rows independently.
pub(crate) const RESIZE_RECOVERY_MIN_PROBE_ROWS: usize = 24;
/// Slack after synchronized output's deadline before a follow-up render, so
/// the terminal can finish its batch.
pub(crate) const SYNCHRONIZED_OUTPUT_FLUSH_MARGIN: Duration = Duration::from_millis(5);
/// Maximum number of synchronized-output timeout flushes waiting in or using
/// Tokio's blocking pool. These flushes can wait for ordered filesystem
/// effects, so they must leave pool capacity for unrelated server work.
pub(crate) const SYNCHRONIZED_OUTPUT_FLUSH_LIMIT: usize = 8;
/// Maximum number of detector ticks running in Tokio's blocking pool at once,
/// across every pane. A pane never holds more than one: its detection task
/// awaits each tick before scheduling the next, so a tick stalled on a hung
/// mount holds one permit and stalls only its own pane. The cap is therefore
/// set well above the handful of panes one hung mount is likely to stall, so
/// those cannot starve detection everywhere else, and well below Tokio's
/// default blocking-pool ceiling (512 threads), so stalled ticks leave room
/// for the server's other blocking work. A lower cap trades that isolation
/// for fewer threads; it is not a throughput setting.
pub(crate) const DETECTOR_TICK_CONCURRENCY: usize = 64;
/// Rows a chunked scrollback scan (copy-mode search) reads per hold of the
/// terminal lock. Between chunks the lock is released so the PTY reader,
/// rendering and detection are never stalled behind a scan of the whole
/// scrollback.
pub(crate) const SCAN_CHUNK_ROWS: usize = 2048;
/// Screens of history, at the resized height, a resize may step a blank
/// scrolled-back viewport toward live output looking for text; never fewer
/// than `RESIZE_RECOVERY_MIN_PROBE_ROWS` rows. The walk holds the terminal lock,
/// so it stops here however deep the history is.
pub(crate) const RESIZE_RECOVERY_PROBE_SCREENS: usize = 8;

// Copy-mode motions, which read the terminal under its lock.

/// The farthest a copy-mode paragraph motion looks for a blank row. The scan
/// holds the terminal lock, so it is bounded however deep the history is.
pub(crate) const MAX_PARAGRAPH_MOTION_ROWS: usize = 1000;
/// Rows a copy-mode word motion first reads from its start, so an ordinary
/// motion formats only a small window under the terminal lock. The window
/// doubles, up to `MAX_WORD_MOTION_ROWS`, while the answer may lie past its
/// edge: no target inside it yet, or a word continuing across a soft wrap
/// there.
pub(crate) const WORD_MOTION_INITIAL_WINDOW_ROWS: usize = 64;
/// The most rows a copy-mode word motion reads, counting its start row. The
/// read holds the terminal lock, so it is bounded however deep the history is
/// (with the doublings, under twice this many rows are formatted in all). A
/// motion treats the window's far edge at this size as the end of the history:
/// with no target inside it the motion does not move, and a word that runs on
/// past it ends (or, backward, starts) at that edge. A power-of-two multiple
/// of `WORD_MOTION_INITIAL_WINDOW_ROWS`, so the last doubling lands on it.
pub(crate) const MAX_WORD_MOTION_ROWS: usize = 1024;

// OSC evidence retained or logged from untrusted terminal output.

/// Largest OSC body the OSC debug log reports; a longer one is skipped, which
/// bounds the log entries built from untrusted terminal output.
pub(crate) const MAX_OSC_BODY_BYTES: usize = 4096;
/// Maximum debug payload characters logged from an OSC body; enough context
/// for diagnosis without allowing a large log entry.
pub(crate) const MAX_OSC_DEBUG_CHARS: usize = 512;
/// Maximum agent OSC title characters retained from untrusted output.
/// What counts as a displayable character is `shepr_term::title`'s rule.
pub(crate) const AGENT_OSC_MAX_CHARS: usize = 256;

// Pane launch and ending.

/// How long a pane launch whose child has exited still waits for that child's
/// status channel. A child that connected before exiting is already in the
/// listener's queue and is routed at once; this only bounds the wait for one
/// that never connected (a failure before its first report).
pub(crate) const LAUNCH_STATUS_AFTER_EXIT: Duration = Duration::from_secs(1);
/// How long a launch still unsettled when its pane ended with the child
/// possibly alive (a failed PTY reader, a failed wait) may take to settle
/// before it is settled as unconfirmed. Lets a failure report already sent
/// arrive, without letting a child stuck in its chdir keep the pane open.
pub(crate) const LAUNCH_SETTLE_AFTER_PANE_END: Duration = Duration::from_secs(1);
/// How long a pane whose terminal closed waits for its child watcher to
/// report the exit before ending the pane on its own. A child that exits
/// closes its terminal moments before it is reaped, so this normally runs out
/// only for a child that closed its terminal and kept running.
pub(crate) const TERMINAL_CLOSED_EXIT_GRACE: Duration = Duration::from_secs(2);
/// Grace per pane teardown signal before escalating to the next signal.
const PANE_TEARDOWN_STEP: Duration = Duration::from_millis(250);
/// Poll cadence when pidfd readiness cannot be used by the async runtime.
pub(crate) const CHILD_WAIT_FALLBACK_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Escalation sequence for a pane session, using a grace interval after each
/// signal before the next round.
pub(crate) const PANE_TEARDOWN_STEPS: [(shepr_platform::Signal, Duration); 3] = [
    (shepr_platform::Signal::Hangup, PANE_TEARDOWN_STEP),
    (shepr_platform::Signal::Terminate, PANE_TEARDOWN_STEP),
    (shepr_platform::Signal::Kill, PANE_TEARDOWN_STEP),
];
/// Sum of the grace intervals in `PANE_TEARDOWN_STEPS`. Session scans and
/// system calls are outside this signal-time budget.
pub(crate) const PANE_TEARDOWN_BUDGET: Duration = {
    let mut budget = Duration::ZERO;
    let mut index = 0;
    while index < PANE_TEARDOWN_STEPS.len() {
        budget = budget.saturating_add(PANE_TEARDOWN_STEPS[index].1);
        index += 1;
    }
    budget
};
/// Additional shutdown allowance for session scans: one signal-time budget
/// per step. `/proc` walks and system calls have no finite bound, so this is a
/// wait allowance, not a guarantee that teardown finishes within it.
const PANE_TEARDOWN_SCAN_ALLOWANCE: Duration = {
    let mut allowance = Duration::ZERO;
    let mut index = 0;
    while index < PANE_TEARDOWN_STEPS.len() {
        allowance = allowance.saturating_add(PANE_TEARDOWN_BUDGET);
        index += 1;
    }
    allowance
};
/// Server-exit wait allowance derived from the teardown's own signal steps.
pub(crate) const PANE_TEARDOWN_WAIT: Duration =
    PANE_TEARDOWN_BUDGET.saturating_add(PANE_TEARDOWN_SCAN_ALLOWANCE);

// Session persistence: recovery copies and file size bounds.

/// Interval between layout snapshots; this gives recovery points without
/// writing a new file for every save.
pub(crate) const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Nominal span used to derive the maximum number of recovery points, assuming
/// one eligible changed-layout copy per interval.
const SNAPSHOT_RECOVERY_WINDOW: Duration = Duration::from_secs(12 * 60 * 60);
/// Maximum number of recovery points retained. At the 15-minute interval this
/// allows about 12 hours when a changed layout is copied each interval;
/// unchanged layouts are not copied, so the retained points can span longer.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the snapshot count is a few dozen, which fits any usize"
)]
pub(crate) const SNAPSHOT_LIMIT: usize =
    (SNAPSHOT_RECOVERY_WINDOW.as_secs() / SNAPSHOT_INTERVAL.as_secs()) as usize;
/// Copies retained in the separate backup directory; this is a short
/// fallback trail beside the longer snapshot history.
pub(crate) const BACKUP_LIMIT: usize = 3;
/// Name attempts per recovery timestamp. The data directory lease admits one
/// writer, so a name that is already taken is a leftover, not a concurrent
/// writer. The loop skips such names. The publish is not exclusive against a
/// second writer and does not need to be.
pub(crate) const RECOVERY_SEQUENCE_LIMIT: usize = 128;
/// The session layout file's size bound, for saves and for the reads restore
/// and snapshot recovery make, so a damaged file cannot allocate without limit.
pub(crate) const MAX_SESSION_FILE_BYTES: usize = 64 * 1024 * 1024;
/// Maximum symlink hops when finding a writable session path; bounds cycles
/// while allowing an ordinary chain of user-managed links.
pub(crate) const MAX_SESSION_PATH_SYMLINK_HOPS: usize = 16;
