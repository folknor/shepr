use std::time::Duration;

/// Maximum runtime configured for an installed agent hook. The deadline allows
/// a cold hook interpreter to report state while bounding the agent's wait.
pub(crate) const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum symbolic-link hops followed while resolving an integration config.
/// Matching Linux path resolution's traversal ceiling turns cycles or
/// pathological chains into a prompt error.
pub(crate) const MAX_CONFIG_SYMLINK_DEPTH: usize = 40;

/// Bytes reserved for the opening and closing quotes in a TOML basic string.
/// Escapes may expand beyond this estimate and the string grows as needed.
pub(crate) const TOML_BASIC_STRING_DELIMITER_BYTES: usize = 2;

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
/// Delay Idle across a quick OMP turn transition.
#[cfg(test)]
pub(crate) const OMP_IDLE_DEBOUNCE: Duration = Duration::from_millis(250);
/// Retain Working while OMP may start a provider retry.
#[cfg(test)]
pub(crate) const OMP_RETRY_GRACE: Duration = Duration::from_millis(2500);
