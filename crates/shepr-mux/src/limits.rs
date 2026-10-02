//! Runtime budgets and retention limits for mux-owned state.

use std::time::Duration;

/// The public number of the first workspace a process allocates. Public
/// numbers are one-based; zero spells no workspace ID.
pub(crate) const FIRST_WORKSPACE_NUMBER: usize = 1;

/// How long one Git probe may run before it is killed. This bounds hung Git
/// reads so they cannot stall workspace and sidebar updates.
pub(crate) const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Polling interval while waiting for a Git probe and its output readers. The
/// interval keeps exit detection responsive without a busy loop.
pub(crate) const GIT_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(5);

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
/// Maximum distinct hook sources tracked by a terminal, preventing arbitrary
/// source names from growing the ordering map without bound.
pub(crate) const MAX_HOOK_REPORT_SOURCES: usize = 64;
/// Maximum stale lifecycle sessions remembered per hook source, bounding
/// deduplication memory while retaining recent reports.
pub(crate) const MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE: usize = 64;

/// Consecutive process misses required before dropping an identified agent;
/// transient /proc gaps must not erase its state.
pub(crate) const AGENT_MISS_CONFIRMATION_ATTEMPTS: u8 = 6;
/// Recheck cadence for an already identified process, limiting probe work.
pub(crate) const PROCESS_RECHECK_IDENTIFIED: Duration = Duration::from_secs(5);
/// Recheck cadence when no foreground group is available; this condition is
/// unlikely to resolve quickly and should not spin.
pub(crate) const PROCESS_RECHECK_MISSING_FOREGROUND_GROUP: Duration = Duration::from_secs(30);
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
/// Time allowed for a restored agent to appear after its resume launch.
pub(crate) const AGENT_RESUME_DETECTION_HOLD: Duration = Duration::from_secs(30);
/// A restored pane holds absence for the same interval as agent resume, so
/// detection cannot clear the agent before its process has time to appear.
pub(crate) const AGENT_ABSENCE_STARTUP_HOLD: Duration = AGENT_RESUME_DETECTION_HOLD;
/// Default screen depth sampled for agent detection when no caller supplies
/// one; it covers a conventional terminal viewport.
pub(crate) const DEFAULT_DETECTION_ROWS: usize = 24;
/// Slack after synchronized output's deadline before a follow-up render, so
/// the terminal can finish its batch.
pub(crate) const SYNCHRONIZED_OUTPUT_FLUSH_MARGIN: Duration = Duration::from_millis(5);
/// Rows a chunked history scan reads per hold of the terminal lock. Between
/// chunks the lock is released so the PTY reader, rendering and detection
/// are never stalled behind a scan of the whole scrollback.
pub(crate) const SCAN_CHUNK_ROWS: u64 = 2048;
/// The most rows a merged history chunk covers. Eviction drops a chunk whole
/// and formats the rows of it that survive again under one lock hold, so this
/// bounds that rework (a history at its limit evicts on nearly every save, and
/// each would redo the whole oldest chunk). With `MERGE_MAX_BYTES` it also sets
/// the chunk count: two neighbours that could still merge do not exist, so a
/// cache of `n` rows holds about `2 n / MERGE_MAX_ROWS` chunks at most, however
/// often it was saved.
pub(crate) const MERGE_MAX_ROWS: u64 = 256;
/// The most text a merged history chunk holds. Merging copies both texts, and
/// a save that adds a few rows to a small last chunk copies that chunk again;
/// this caps the copy at a size that costs far less than the formatting of the
/// rows that caused it, which happens under the terminal lock and the copy does
/// not.
pub(crate) const MERGE_MAX_BYTES: usize = 64 * 1024;
/// Copy-mode punctuation treated as word boundaries, matching shell-style
/// punctuation rather than consuming it as part of words.
pub(crate) const COPY_MODE_WORD_SEPARATORS: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~";

/// Maximum OSC body buffered before discarding it; bounds untrusted terminal
/// output and the debug collector's retained memory.
pub(crate) const MAX_OSC_BODY_BYTES: usize = 4096;
/// Maximum agent OSC title/progress characters retained from untrusted output.
pub(crate) const AGENT_OSC_MAX_CHARS: usize = 256;
/// Maximum debug payload characters logged from an OSC body; enough context
/// for diagnosis without allowing a large log entry.
pub(crate) const MAX_OSC_DEBUG_CHARS: usize = 512;

/// Maximum bytes read from one loose Git ref file; far above any real ref,
/// it bounds the read of a corrupt or hostile file.
pub(crate) const MAX_GIT_REF_FILE_BYTES: usize = 64 * 1024;
/// Retry delay after Git status refresh fails, avoiding repeated filesystem
/// and subprocess work for a broken or unavailable checkout.
pub(crate) const GIT_STATUS_RETRY_DELAY: Duration = Duration::from_secs(30);

/// Interval between layout snapshots; this gives recovery points without
/// writing a new file for every save.
pub(crate) const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Recovery span retained at the snapshot cadence; this covers an overnight
/// failure while keeping the snapshot directory bounded.
pub(crate) const SNAPSHOT_RECOVERY_WINDOW: Duration = Duration::from_secs(12 * 60 * 60);
/// Number of recovery points retained across the bounded recovery span.
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
/// writer: for example a history copy whose layout copy was never published,
/// which pruning keeps when it is not older than the newest layout copy. The
/// loop skips such names. The publish is not exclusive against a second
/// writer and does not need to be.
pub(crate) const RECOVERY_SEQUENCE_LIMIT: usize = 128;
/// This is the session-history writer's file budget and restore uses the same
/// bound. `serialize_history` trims pane text to it; if the workspace shape
/// alone is larger, it writes a compact history with no pane entries. The
/// fingerprint in that compact form is a fixed SHA-256 digest.
pub(crate) const MAX_SESSION_HISTORY_FILE_BYTES: usize = 256 * 1024 * 1024;
/// The session layout file's size bound, for saves and for the reads restore
/// and snapshot recovery make, so a damaged file cannot allocate without limit.
pub(crate) const MAX_SESSION_FILE_BYTES: usize = 64 * 1024 * 1024;
/// Maximum symlink hops when finding a writable session path; bounds cycles
/// while allowing an ordinary chain of user-managed links.
pub(crate) const MAX_SESSION_PATH_SYMLINK_HOPS: usize = 16;
/// Grace per pane teardown signal before escalating to the next signal.
pub(crate) const PANE_TEARDOWN_STEP: Duration = Duration::from_millis(250);
/// Total teardown wait: the sum of the grace intervals in `PANE_TEARDOWN_STEPS`.
pub(crate) const PANE_TEARDOWN_BUDGET: Duration = {
    let mut budget = Duration::ZERO;
    let mut index = 0;
    while index < PANE_TEARDOWN_STEPS.len() {
        budget = budget.saturating_add(PANE_TEARDOWN_STEPS[index].1);
        index += 1;
    }
    budget
};
/// How long a pane whose terminal closed waits for its child watcher to
/// report the exit before ending the pane on its own. A child that exits
/// closes its terminal moments before it is reaped, so this normally runs out
/// only for a child that closed its terminal and kept running.
pub(crate) const TERMINAL_CLOSED_EXIT_GRACE: Duration = Duration::from_secs(2);
/// Escalation sequence for a pane session, using a grace interval after each
/// signal before the next round.
pub(crate) const PANE_TEARDOWN_STEPS: [(shepr_platform::Signal, Duration); 3] = [
    (shepr_platform::Signal::Hangup, PANE_TEARDOWN_STEP),
    (shepr_platform::Signal::Terminate, PANE_TEARDOWN_STEP),
    (shepr_platform::Signal::Kill, PANE_TEARDOWN_STEP),
];

/// A detector release remains provisional while a session-wide kill can still
/// reach the shell. The detector republishes after this live-shell interval.
pub(crate) const AGENT_PROCESS_EXIT_RELEASE_GRACE: Duration = Duration::from_millis(750);
/// How long a pane launch whose child has exited still waits for that child's
/// status channel. A child that connected before exiting is already in the
/// listener's queue and is routed at once; this only bounds the wait for one
/// that never connected (a failure before its first report).
pub(crate) const LAUNCH_STATUS_AFTER_EXIT: Duration = Duration::from_secs(1);
/// How often a pane launch checks whether its child exited when it cannot
/// watch the child's pidfd (the dup failed). Only that fallback polls.
pub(crate) const LAUNCH_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(100);
