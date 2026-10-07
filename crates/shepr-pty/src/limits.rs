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

/// Size of one pane launch status record: a kind word, a reserved word and a
/// 64-bit value (ticket, cwd index or errno). Fixed so the child writes it from
/// a stack array and the reader can reject a truncated or oversized one.
pub(crate) const LAUNCH_STATUS_RECORD_BYTES: usize = 16;

/// How long the status listener waits for a new connection's hello. Our own
/// pane child sends it right after connecting; the bound only keeps a stray
/// local connection from holding its descriptor open. Hellos are awaited
/// together, so a silent connection delays no other.
pub(crate) const LAUNCH_HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a status connection that arrived before its launch registered
/// waits for that registration. The server registers right after the fork, so
/// one that waits this long belongs to no launch.
pub(crate) const LAUNCH_PARKED_CONNECTION_TTL: std::time::Duration =
    std::time::Duration::from_secs(30);

/// How long the launch status listener waits before accepting or polling
/// again after the process ran out of fds or memory. Long enough not to spin while the
/// shortage lasts, short enough that waiting children are not held long.
pub(crate) const LAUNCH_ACCEPT_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_millis(100);

/// Highest signal number whose disposition a pane child resets before exec.
/// Linux architectures use numbers up to 64 or 128; unsupported ones report
/// EINVAL and are skipped.
pub(crate) const MAX_SIGNAL_NUMBER: libc::c_int = 128;

/// Stack bytes supplied to each `getdents64` call while closing inherited
/// descriptors. A fixed chunk avoids a heap allocation and keeps each syscall
/// bounded while enumerating `/proc/self/fd`.
pub(crate) const GETDENTS_READ_BUFFER_BYTES: usize = 4 * KIBIBYTE_BYTES;

/// Idle timeout for the PTY actor's `poll` call. Wake-pipe and PTY readiness
/// drive normal responsiveness; this timeout catches missed wakes and is the
/// cadence for noticing a terminal core poisoned on another thread while the
/// pane has no IO.
pub(crate) const ACTOR_IDLE_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Shortest idle poll a caller may configure. A zero timeout would turn the
/// actor's `poll` into a busy loop, so a shorter request is raised to this.
pub(crate) const ACTOR_IDLE_POLL_MIN: std::time::Duration = std::time::Duration::from_millis(1);

/// Total queued PTY input and terminal-reply bytes allowed while other items
/// are outstanding. A lone oversized item is admitted by the inbox; this
/// budget bounds accumulation without rejecting one paste when the queue is
/// empty.
pub(crate) const ACTOR_INBOX_MAX_BYTES: usize = 256 * KIBIBYTE_BYTES;

/// Maximum queued PTY inbox items. This separately bounds per-item bookkeeping
/// when many small writes consume little of the byte budget.
pub(crate) const ACTOR_INBOX_MAX_ITEMS: usize = 1_024;

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
