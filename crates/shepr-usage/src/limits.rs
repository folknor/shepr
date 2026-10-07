//! Timing and size bounds for usage tracking.

use std::time::Duration;

/// How often each account's usage is polled while the server is active.
/// The endpoints are undocumented and throttled per account, so this stays
/// well above anything a person needs to watch a five-hour window.
pub(crate) const USAGE_POLL_CADENCE: Duration = Duration::from_secs(5 * 60);

/// Largest share of the cadence added as jitter to a successful poll's next
/// due time. Jitter only ever delays, never brings a poll forward.
pub(crate) const USAGE_POLL_JITTER_PERCENT: u32 = 20;

/// Least time between two request starts to one host by this worker,
/// identity and profile requests included.
pub(crate) const HOST_REQUEST_SPACING: Duration = Duration::from_secs(5);

/// How often a Claude account's profile (plan metadata) is refreshed once its
/// identity is known. Identity bootstrap for a new credential is separate.
pub(crate) const PROFILE_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Successive delays after a 429, the last repeating. A positive
/// Retry-After lengthens a step but never shortens it.
pub(crate) const THROTTLE_LADDER: [Duration; 5] = [
    Duration::from_secs(5 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(20 * 60),
    Duration::from_secs(40 * 60),
    Duration::from_secs(60 * 60),
];

/// First delay after a network, transport or server error; it doubles per
/// consecutive failure up to [`FAILURE_BACKOFF_MAX`].
pub(crate) const FAILURE_BACKOFF_START: Duration = Duration::from_secs(60);

/// Longest delay after consecutive network, transport or server errors.
pub(crate) const FAILURE_BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);

/// The longest positive Retry-After honoured; a larger hint is clamped so a
/// bogus header cannot park an account for days.
pub(crate) const RETRY_AFTER_MAX: Duration = Duration::from_secs(6 * 60 * 60);

/// How long after a window's reset time one extra poll runs, so the new
/// window shows without waiting for the cadence. Backoff still applies.
pub(crate) const RESET_FOLLOW_UP: Duration = Duration::from_secs(15);

/// Total budget of one HTTP request: curl's own `--max-time` and the
/// supervisor's deadline both derive from it.
pub(crate) const REQUEST_BUDGET: Duration = Duration::from_secs(10);

/// Largest response body accepted. Real usage and profile bodies are a few
/// kilobytes; anything bigger is refused, not parsed as a prefix.
pub(crate) const MAX_RESPONSE_BODY_BYTES: usize = 256 * 1024;

/// Largest response header stream accepted, every block included.
pub(crate) const MAX_RESPONSE_HEADER_BYTES: usize = 64 * 1024;

/// Most response blocks (interim 1xx plus the final one) in a header stream.
pub(crate) const MAX_RESPONSE_BLOCKS: usize = 8;

/// Longest single header line accepted.
pub(crate) const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// Bytes of curl's stderr kept for diagnostics; it is never logged verbatim.
pub(crate) const MAX_CURL_STDERR_BYTES: usize = 4 * 1024;

/// Bytes of `curl --version` output read by the capability probe.
pub(crate) const MAX_CURL_VERSION_BYTES: usize = 16 * 1024;

/// Budget of the capability probe run.
pub(crate) const CURL_PROBE_BUDGET: Duration = Duration::from_secs(5);

/// How long the worker waits before probing curl again after a failed probe.
pub(crate) const CURL_PROBE_RETRY: Duration = Duration::from_secs(10 * 60);

/// Largest credential file read. Agent credential files are small JSON
/// documents; a larger one is refused, not parsed as a prefix.
pub(crate) const MAX_CREDENTIAL_FILE_BYTES: u64 = 256 * 1024;

/// How often every source's credentials are re-read while active.
pub(crate) const CREDENTIAL_REREAD_INTERVAL: Duration = Duration::from_secs(30);

/// How long a credential read may take before its source is reported
/// stalled. The reader thread keeps its capacity until it really finishes.
pub(crate) const CREDENTIAL_READ_DEADLINE: Duration = Duration::from_secs(5);

/// Most credential reader threads alive at once, stalled ones included.
pub(crate) const MAX_CREDENTIAL_READERS: usize = 4;

/// Active time a source may keep failing to read before it is marked
/// unreadable. Requests already stop at the first failure.
pub(crate) const UNREADABLE_GRACE: Duration = Duration::from_secs(60);

/// Most remembered sources kept on disk.
pub(crate) const MAX_REMEMBERED_SOURCES: usize = 64;

/// Largest remembered-sources file read.
pub(crate) const MAX_REGISTRY_FILE_BYTES: u64 = 64 * 1024;

/// Longest the worker sleeps with nothing due, so a stalled reader or a
/// missed wake is noticed.
pub(crate) const WORKER_IDLE_WAKE: Duration = Duration::from_secs(5);

/// Shortest the worker sleeps between passes: a guard against a timer that
/// keeps falling due without its work being able to start.
pub(crate) const WORKER_MIN_SLEEP: Duration = Duration::from_millis(25);

/// Most events one worker pass handles before it schedules, so a burst of
/// completions cannot starve scheduling; the rest follow at once.
pub(crate) const MAX_EVENTS_PER_PASS: usize = 64;

/// Most refused generations remembered after their credentials are gone;
/// past it the oldest are forgotten.
pub(crate) const MAX_TOMBSTONES: usize = 256;

/// Gates kept before idle ones (not in backoff) are forgotten. A gate in
/// backoff is never dropped.
pub(crate) const MAX_GATES: usize = 512;
