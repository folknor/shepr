//! Terminal identities, allocated where terminals are created.

use std::num::NonZeroU64;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shepr_protocol::TerminalId;

// Starting at one keeps generated terminal ID suffixes nonzero.
static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(1);

/// The stamp every terminal ID of this process carries, taken at the first
/// allocation. It only has to tell one server lifetime from another (the
/// counter restarts with each process), so an id remembered from an earlier
/// server never names a new terminal. It is identity, not a time any
/// decision reads, which is why it is sampled here once rather than passed in
/// through the clock seam.
static TERMINAL_ID_STAMP: OnceLock<Result<Duration, Duration>> = OnceLock::new();

/// A terminal ID no other terminal of this process has. Every terminal a
/// workspace, a split or a restore creates takes its ID from here.
pub fn allocate_terminal_id() -> TerminalId {
    let stamp =
        *TERMINAL_ID_STAMP.get_or_init(|| match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => Ok(duration),
            Err(error) => Err(error.duration()),
        });
    // One allocation per terminal never exhausts a u64; wrapping would
    // repeat an earlier id, so exhaustion is refused rather than wrapped. The
    // counter starts at one and only grows, so it is never zero.
    let counter = NEXT_TERMINAL_ID
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |counter| {
            counter.checked_add(1)
        })
        .ok()
        .and_then(NonZeroU64::new);
    let Some(counter) = counter else {
        panic!("terminal id allocation counter exhausted");
    };
    TerminalId::from_clock_and_counter(stamp, counter)
}

#[cfg(test)]
mod tests {
    use super::allocate_terminal_id;
    use shepr_protocol::TerminalId;

    #[test]
    fn allocated_terminal_ids_parse_back_and_differ() {
        let first = allocate_terminal_id();
        let second = allocate_terminal_id();

        assert_ne!(first, second);
        assert_eq!(first.as_str().parse::<TerminalId>(), Ok(first.clone()));
        assert_eq!(second.as_str().parse::<TerminalId>(), Ok(second));
    }
}
