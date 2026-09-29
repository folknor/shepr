//! Runtime budgets and retention limits for mux-owned state.

use std::time::Duration;

/// Heuristic fraction of comparable viewport rows needed to reuse a read
/// snapshot. A high overlap tolerates a small changing status area while
/// rejecting a substantially different screen; it is not a measured rate.
pub(crate) const SIMILAR_VIEWPORT_RATIO_PERCENT: usize = 70;
/// Heuristic minimum fraction of overlapping nonblank rows needed to align
/// upward history. A lower overlap tolerates pinned headers and changing status
/// rows while still requiring more than an isolated accidental match.
pub(crate) const MIN_ALIGNMENT_RATIO_PERCENT: usize = 30;
/// Shared percent scale for both screen similarity thresholds, so ratios stay
/// readable as whole percentages.
pub(crate) const PERCENT_DENOMINATOR: usize = 100;

/// Hook reports are ordered per source by the `seq` each hook process takes
/// from its own wall clock (nanoseconds for the shell/python hooks,
/// microseconds for the JS plugins; only ever compared within one source).
/// A report whose `seq` is not above the last accepted one is normally a
/// straggler from a racing hook process and is dropped. Hook processes race
/// over milliseconds, though; a non-increasing `seq` arriving this long after
/// the source's last accepted report means the clock stepped backwards (NTP,
/// resume, a manual change), and dropping would lose every report until the
/// clock caught up again. Such a report is accepted and re-anchors the
/// source's sequence.
pub(crate) const HOOK_SEQUENCE_REANCHOR_AFTER: Duration = Duration::from_secs(5);
/// Maximum distinct hook sources tracked by a terminal, preventing arbitrary
/// source names from growing the ordering map without bound.
pub(crate) const MAX_HOOK_REPORT_SOURCES: usize = 64;
/// Maximum stale lifecycle sessions remembered per hook source, bounding
/// deduplication memory while retaining recent reports.
pub(crate) const MAX_STALE_FULL_LIFECYCLE_HOOK_SESSIONS_PER_SOURCE: usize = 64;
/// Distinct metadata sources one terminal keeps, both for live presentation
/// metadata and for the per-source report sequences. Any process in the pane
/// can report metadata under a source name of its choosing, so without a cap
/// a script that invents a new name per report grows these maps forever.
pub(crate) const MAX_METADATA_SOURCES: usize = 64;
/// Maximum sequence sources tracked for workspace metadata tokens, bounding
/// names supplied by child processes.
pub const MAX_SEQUENCE_SOURCES: usize = 32;

/// Time to suppress reacquisition after a release, allowing process state to
/// settle before the same agent can be reported again.
pub(crate) const RELEASE_REACQUIRE_SUPPRESSION: Duration = Duration::from_secs(1);
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
/// Probe cadence during a pending release or transient color override, when
/// a visible state change is expected immediately.
pub(crate) const PROCESS_RECHECK_TRANSIENT: Duration = Duration::from_millis(50);
/// Probe cadence when no agent is identified, balancing acquisition latency
/// against repeated process scans.
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
pub const AGENT_RESUME_DETECTION_HOLD: Duration = Duration::from_secs(30);
/// A restored pane holds absence for the same interval as agent resume, so
/// detection cannot clear the agent before its process has time to appear.
pub(crate) const AGENT_ABSENCE_STARTUP_HOLD: Duration = AGENT_RESUME_DETECTION_HOLD;
/// Delay before the detector first polls a newly launched pane, giving the
/// shell time to put initial output on the screen.
pub(crate) const INITIAL_DETECTION_DELAY: Duration = Duration::from_millis(50);
/// Tries to read a stable screen snapshot across concurrent PTY updates.
/// Retries tolerate a brief write without spinning indefinitely.
pub(crate) const SCREEN_SNAPSHOT_READ_ATTEMPTS: usize = 3;

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
/// Name attempts per recovery timestamp. The copy is created exclusively, so a
/// concurrent writer that picked the same timestamp moves on to the next one.
pub(crate) const RECOVERY_SEQUENCE_LIMIT: usize = 128;
/// This is the session-history writer's file budget and restore uses the same
/// bound. `serialize_history` trims pane text to it; if the workspace/tab shape
/// alone is larger, it writes a compact history with no pane entries. The
/// fingerprint in that compact form is a fixed SHA-256 digest.
pub(crate) const MAX_SESSION_HISTORY_FILE_BYTES: usize = 256 * 1024 * 1024;
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
/// Escalation sequence for a pane session, using a grace interval after each
/// signal before the next round.
pub(crate) const PANE_TEARDOWN_STEPS: [(shepr_platform::Signal, Duration); 3] = [
    (shepr_platform::Signal::Hangup, PANE_TEARDOWN_STEP),
    (shepr_platform::Signal::Terminate, PANE_TEARDOWN_STEP),
    (shepr_platform::Signal::Kill, PANE_TEARDOWN_STEP),
];
