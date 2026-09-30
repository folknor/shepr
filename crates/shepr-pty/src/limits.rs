use shepr_core::limits::KIBIBYTE_BYTES;

/// Initial scratch allocation passed to `getpwuid_r` for one passwd lookup.
/// A small initial buffer avoids a large allocation for the common short
/// entry; it grows on `ERANGE` up to `PASSWD_BUFFER_MAX_BYTES`.
pub(crate) const PASSWD_BUFFER_INITIAL_BYTES: usize = KIBIBYTE_BYTES;

/// Maximum scratch allocation for one `getpwuid_r` lookup. The ceiling accepts
/// unusually large NSS records while preventing unbounded growth.
pub(crate) const PASSWD_BUFFER_MAX_BYTES: usize = 64 * KIBIBYTE_BYTES;

/// Geometric growth factor for the `getpwuid_r` scratch buffer. Doubling keeps
/// retries logarithmic while the separate maximum bounds total allocation.
pub(crate) const PASSWD_BUFFER_GROWTH_FACTOR: usize = 2;

/// Stack bytes supplied to each `getdents64` call while closing inherited
/// descriptors. A fixed chunk avoids a heap allocation and keeps each syscall
/// bounded while enumerating `/proc/self/fd`.
pub(crate) const GETDENTS_READ_BUFFER_BYTES: usize = 4 * KIBIBYTE_BYTES;

/// Idle timeout for the PTY actor's `poll` call. Wake-pipe and PTY readiness
/// drive normal responsiveness; this is only a fallback for a missed
/// wake.
pub(crate) const ACTOR_IDLE_POLL_MS: i32 = 1_000;

/// Total queued PTY input and terminal-reply bytes allowed while other items
/// are outstanding. A lone oversized item is admitted by the inbox; this
/// budget bounds accumulation without rejecting one paste when the queue is
/// empty.
pub(crate) const ACTOR_INBOX_MAX_BYTES: usize = 256 * KIBIBYTE_BYTES;

/// Maximum queued PTY inbox items. This separately bounds per-item bookkeeping
/// when many small writes consume little of the byte budget.
pub(crate) const ACTOR_INBOX_MAX_ITEMS: usize = 1_024;

/// Minimum positive poll timeout, in milliseconds, after rounding a deadline.
/// A sub-millisecond remainder must not become a zero-time busy poll.
pub(crate) const MIN_POLL_TIMEOUT_MS: u128 = 1;

/// Maximum write operations one actor pump performs before polling again. The
/// cap prevents continuous input from starving reads of child output.
pub(crate) const MAX_WRITE_STEPS_PER_PUMP: usize = 64;

/// Maximum PTY chunks drained after a write failure. The budget lets pending
/// output drain while stopping a continuously writing child from extending the
/// best-effort flush indefinitely.
pub(crate) const MAX_WRITE_FAILURE_DRAIN_CHUNKS: usize = 1_024;

/// Bytes read from the PTY master per actor read step. The chunk amortizes
/// syscalls while bounding each stack buffer and each unit of drain work.
pub(crate) const PTY_READ_BUFFER_BYTES: usize = 8 * KIBIBYTE_BYTES;

/// Bytes read from the wake pipe per drain step. Wake-byte contents are
/// ignored; the fixed buffer clears signal bursts without growing stack use.
pub(crate) const WAKE_PIPE_READ_BUFFER_BYTES: usize = 64;
