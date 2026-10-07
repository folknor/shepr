use std::time::Duration;

/// Maximum runtime configured for an installed agent hook. The deadline allows
/// a cold hook interpreter to report state while bounding the agent's wait.
pub(crate) const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum symbolic-link hops followed while resolving an integration config.
/// Matching Linux path resolution's traversal ceiling turns cycles or
/// pathological chains into a prompt error.
pub(crate) const MAX_CONFIG_SYMLINK_DEPTH: usize = 40;

/// Largest agent config or installed asset an integration reads. Agent
/// configs carry user additions but are never bulk data; the cap keeps a
/// runaway or hostile file from being read whole into memory at launch.
pub(crate) const MAX_INTEGRATION_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Bytes reserved for the opening and closing quotes in a TOML basic string.
/// Escapes may expand beyond this estimate and the string grows as needed.
pub(crate) const TOML_BASIC_STRING_DELIMITER_BYTES: usize = 2;

// The timings below are generated into the hook assets. The JavaScript and
// TypeScript kits read `Date.now()` and `setTimeout` directly, with no clock
// handed in, so their bun tests wait in real time (up to a few seconds), and
// some decoder literals live in the decoders rather than here. Both are
// accepted: a clock seam in the kits buys only faster tests.
//
// The JavaScript reporters also seed their `seq` once per module load, from
// `Date.now() * 1000`, and never re-sample. If the host clock steps backwards
// and the extension then reloads in a live process (Pi `/reload`), the new
// instance's seqs sit below the old instance's, and the server drops them
// until wall time passes the old base. The server's backwards-step re-anchor
// covers reports arriving across the step, not a reload after it. Accepted:
// it needs a backwards clock step and a reload in the same live process, and
// it clears itself as wall time catches up.

/// How long a bundled hook waits to connect to the shepr socket before giving
/// up. Generated into every hook asset, so the agent is never held longer than
/// this by an unreachable server.
#[cfg(test)]
pub(crate) const HOOK_SOCKET_WAIT: Duration = Duration::from_millis(500);

/// Retry spacing for OpenCode TUI lifecycle delivery.
#[cfg(test)]
pub(crate) const TUI_RETRY_WAIT: Duration = Duration::from_millis(500);
/// Deadline for an OpenCode TUI SDK request.
#[cfg(test)]
pub(crate) const TUI_REQUEST_WAIT: Duration = Duration::from_secs(5);
/// Cadence for observing OpenCode's selected route.
#[cfg(test)]
pub(crate) const TUI_ROUTE_POLL: Duration = Duration::from_millis(100);
/// Backoff after an undelivered session selection.
#[cfg(test)]
pub(crate) const TUI_SELECTION_RETRIES: &[Duration] = &[
    Duration::from_millis(100),
    Duration::from_millis(400),
    Duration::from_secs(1),
];
/// Most events an unhydrated OpenCode TUI session context buffers, and most
/// deletion tombstones any context keeps, before it is replaced by a fresh one
/// that takes a new snapshot. Bounds memory while hydration keeps failing.
#[cfg(test)]
pub(crate) const TUI_MAX_RETAINED_EVENTS: usize = 4096;
/// Delay Idle across a quick OMP turn transition.
#[cfg(test)]
pub(crate) const OMP_IDLE_DEBOUNCE: Duration = Duration::from_millis(250);
/// Retain Working while OMP may start a provider retry.
#[cfg(test)]
pub(crate) const OMP_RETRY_GRACE: Duration = Duration::from_millis(2500);
