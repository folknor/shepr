use std::time::Duration;

/// Bytes in one binary kibibyte, used to express PTY buffer budgets.
pub(crate) const KIBIBYTE_BYTES: usize = 1024;

/// Compiled-in shell used when neither the inherited nor passwd shell can be
/// resolved to an executable. It is the baseline POSIX shell path on Linux.
pub(crate) const FALLBACK_SHELL: &str = "/bin/sh";

/// Initial scratch allocation passed to `getpwuid_r` for one passwd lookup.
/// One KiB avoids a large allocation for the common short entry; the buffer
/// grows on `ERANGE` up to `PASSWD_BUFFER_MAX_BYTES`.
pub(crate) const PASSWD_BUFFER_INITIAL_BYTES: usize = KIBIBYTE_BYTES;

/// Maximum scratch allocation for one `getpwuid_r` lookup. The 64 KiB ceiling
/// accepts unusually large NSS records while preventing unbounded growth.
pub(crate) const PASSWD_BUFFER_MAX_BYTES: usize = 64 * KIBIBYTE_BYTES;

/// Geometric growth factor for the `getpwuid_r` scratch buffer. Doubling keeps
/// retries logarithmic while the separate maximum bounds total allocation.
pub(crate) const PASSWD_BUFFER_GROWTH_FACTOR: usize = 2;

/// Stack bytes supplied to each `getdents64` call while closing inherited
/// descriptors. A 4 KiB chunk avoids a heap allocation and keeps each syscall
/// bounded while enumerating `/proc/self/fd`.
pub(crate) const GETDENTS_READ_BUFFER_BYTES: usize = 4 * KIBIBYTE_BYTES;

/// Idle timeout for the PTY actor's `poll` call. Wake-pipe and PTY readiness
/// drive normal responsiveness; one second is only a fallback for a missed
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

/// Initial delay before retrying a failed PTY resize ioctl. The retry schedule
/// doubles from 50 ms to avoid a tight failure loop.
pub(crate) const RESIZE_RETRY_BASE: Duration = Duration::from_millis(50);

/// Maximum delay between retries of a failed PTY resize ioctl. The five-second
/// ceiling keeps persistent kernel failures from pacing retries too slowly.
pub(crate) const RESIZE_RETRY_MAX: Duration = Duration::from_secs(5);

/// Failed resize attempts after which queued terminal replies are released so
/// writes do not remain blocked behind an ioctl. Five failures keep replies
/// held through the first 50, 100, 200, and 400 ms waits, then release them on
/// the fifth failure while later retries continue.
pub(crate) const RESIZE_HOLD_ATTEMPTS: u8 = 5;

/// Minimum positive poll timeout, in milliseconds, after rounding a deadline.
/// A sub-millisecond remainder must not become a zero-time busy poll.
pub(crate) const MIN_POLL_TIMEOUT_MS: u128 = 1;

/// Maximum write operations one actor pump performs before polling again. The
/// cap prevents continuous input from starving reads of child output.
pub(crate) const MAX_WRITE_STEPS_PER_PUMP: usize = 64;

/// Maximum PTY chunks drained after a write failure. With an 8 KiB read chunk,
/// 1024 iterations cap this best-effort flush at 8 MiB of child output.
pub(crate) const MAX_WRITE_FAILURE_DRAIN_CHUNKS: usize = 1_024;

/// Bytes read from the PTY master per actor read step. Eight KiB amortizes
/// syscalls while bounding each stack buffer and each unit of drain work.
pub(crate) const PTY_READ_BUFFER_BYTES: usize = 8 * KIBIBYTE_BYTES;

/// Bytes read from the wake pipe per drain step. Wake-byte contents are
/// ignored; 64 bytes clears a burst of signals without growing stack use.
pub(crate) const WAKE_PIPE_READ_BUFFER_BYTES: usize = 64;
