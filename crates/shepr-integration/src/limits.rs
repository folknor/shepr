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
