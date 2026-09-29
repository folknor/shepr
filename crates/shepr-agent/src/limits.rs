use std::time::Duration;

/// Maximum rules in one manifest. This leaves local overrides well past the
/// size of the bundled manifests while keeping override compilation and
/// per-screen evaluation bounded.
pub(crate) const MAX_RULES_PER_MANIFEST: usize = 128;

/// Maximum nested gate depth, including the root gate. Eight levels cover the
/// bundled gate shapes while bounding recursive validation and evaluation.
pub(crate) const MAX_GATE_DEPTH: usize = 8;

/// Maximum gates in one manifest. At the per-manifest rule ceiling this permits
/// four gates per rule on average while bounding recursive compilation and
/// matching across all rules.
pub(crate) const MAX_TOTAL_GATES: usize = 512;

/// Maximum direct matchers on a gate. The fixed match-state array uses this
/// value, keeping gate evaluation allocation-free and its stack use bounded.
pub(crate) const MAX_MATCHERS_PER_GATE: usize = 32;

/// Maximum distinct regions in one manifest. Detection stores each extracted
/// region in a fixed array, so this bounds both cache size and extraction work.
pub(crate) const MAX_REGIONS_PER_MANIFEST: usize = 32;

/// Maximum matchers across a manifest. This permits eight matchers per rule at
/// the rule ceiling while bounding regex compilation and each screen sample's
/// matching work.
pub(crate) const MAX_TOTAL_MATCHERS: usize = 1024;

/// Maximum characters in one matcher. 512 allows detailed screen patterns
/// while preventing oversized expressions from driving unbounded compile work.
pub(crate) const MAX_MATCHER_CHARS: usize = 512;

/// Maximum characters retained in a manifest evidence preview. 240 is about
/// three 80-column lines, keeping explanations readable and output bounded.
pub(crate) const MAX_MANIFEST_PREVIEW_CHARS: usize = 240;

/// Smallest accepted line count for a counted manifest region. Counted regions
/// must select at least one line to have a useful matching scope.
pub(crate) const MIN_REGION_LINE_COUNT: usize = 1;

/// Largest accepted line count for a counted manifest region. The schema caps
/// the decimal field at the largest value representable by its 16-bit range.
pub(crate) const MAX_REGION_LINE_COUNT: usize = u16::MAX as usize;

/// Upper bound on the number of processes visited while resolving a pane's
/// foreground process-group tree. Foreground-job detection reads `/proc/<pid>/stat`
/// and task/children files for every visited process on a repeated (per-tick/5s)
/// cadence, so an unbounded walk lets accumulated descendants or unreaped zombies
/// under the pane shell grow the server's read-syscall rate and CPU without limit
/// at a constant pane count; the walk runs per pane per tick, so its cost
/// multiplies by the number of panes. The foreground-group leader's subtree and
/// the pane shell's descendants advance round-robin under a shared candidate
/// ceiling, with independent per-root work budgets, so a pathologically large
/// accumulation on either side cannot starve the other. Discovery is best effort
/// once a budget is exhausted.
pub(crate) const FOREGROUND_TREE_SCAN_LIMIT: usize = 512;

/// Number of `/proc/<pid>/task` entries a root's subtree may consume, bounding how
/// far one process's thread count can multiply the walk's work.
pub(crate) const FOREGROUND_TASK_ENTRY_LIMIT: usize = 2_048;

/// Number of `/proc/<pid>/task/<tid>/children` bytes a root's subtree may read,
/// stopping a parent that accumulates unreaped children from growing read work
/// without limit.
pub(crate) const FOREGROUND_CHILD_BYTE_LIMIT: usize = 128 * 1024;

/// Aggregate number of child pids a root's subtree may parse and enqueue, bounding
/// the walk's pending queues and allocations.
pub(crate) const FOREGROUND_CHILD_PID_LIMIT: usize = 2_048;

/// Bytes read from one `/proc/.../children` file per syscall. A 4 KiB chunk
/// amortizes reads without allocating in proportion to the entire child list.
pub(crate) const PROC_CHILDREN_READ_BUFFER_BYTES: usize = 4096;

/// Maximum time allowed for an agent's `--version` command. Version commands
/// should start quickly; five seconds accommodates slow startup while stopping
/// an installer from waiting indefinitely on a broken executable.
pub(crate) const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Delay between nonblocking version-probe polls. Ten milliseconds bounds
/// polling to at most 100 checks per second while keeping timeout response
/// prompt.
pub(crate) const VERSION_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Maximum version-probe stdout bytes retained in memory. This is ample for a
/// version banner while bounding output from a misbehaving executable.
pub(crate) const MAX_VERSION_PROBE_OUTPUT: usize = 64 * 1024;

/// Bytes read from a version-probe pipe per syscall. A 4 KiB chunk keeps stack
/// use fixed while avoiding a syscall for every small portion of a banner.
pub(crate) const VERSION_PROBE_READ_BUFFER_BYTES: usize = 4096;

/// Maximum byte length of an agent session ID accepted from hook reports or
/// saved state. This leaves ample room for opaque IDs while bounding persisted
/// and command-line data.
pub(crate) const MAX_SESSION_ID_LEN: usize = 512;

/// Maximum byte length of an agent session path accepted from hook reports or
/// saved state. The 4 KiB ceiling matches the scale of Linux pathname limits
/// and bounds persisted path data.
pub(crate) const MAX_SESSION_PATH_LEN: usize = 4096;

/// Maximum runtime configured for an installed agent hook. Ten seconds allows
/// a cold hook interpreter to report state while bounding the agent's wait.
pub(crate) const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum prompt needle size searched with fixed stack tables. Thirty-two
/// characters covers the known prompt labels and bounds stack use and scan work.
pub(crate) const MAX_PROMPT_READY_NEEDLE_CHARS: usize = 32;

/// Number of recent screen lines inspected for a prompt-ready marker. Twelve
/// lines cover prompt redraws near the bottom while keeping the scan bounded.
pub(crate) const PROMPT_READY_LOOKBACK_LINES: usize = 12;

/// Maximum attempts to create a sibling temporary file before reporting a
/// name collision. The randomized token and sequence make collisions rare;
/// the finite retry cap prevents a hostile directory from causing an endless
/// install loop.
pub(crate) const TEMP_FILE_ALLOCATION_ATTEMPTS: usize = 128;

/// Maximum symbolic-link hops followed while resolving an integration config.
/// Forty matches Linux path resolution's traversal ceiling and turns cycles or
/// pathological chains into a prompt error.
pub(crate) const MAX_CONFIG_SYMLINK_DEPTH: usize = 40;

/// Maximum bytes in the UTF-8 encoding of one Unicode scalar value. The
/// detector uses this as scratch-buffer capacity for `char::encode_utf8`.
pub(crate) const UTF8_MAX_BYTES_PER_CODEPOINT: usize = 4;

/// Bytes reserved for the opening and closing quotes in a TOML basic string.
/// Escapes may expand beyond this estimate and the string grows as needed.
pub(crate) const TOML_BASIC_STRING_DELIMITER_BYTES: usize = 2;
