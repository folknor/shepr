//! The suspend-aware monotonic clock.

use std::io;

/// Nanoseconds on `CLOCK_BOOTTIME`. Unlike the monotonic clock behind
/// `Instant`, this one keeps counting while the host is suspended, so an idle
/// deadline measured on it expires across a suspend and a short maintenance
/// wake can act on it.
pub fn boot_time_nanos() -> io::Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime(2) writes one timespec into a live local.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let secs = u64::try_from(time.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(time.tv_nsec).unwrap_or(0);
    Ok(secs * 1_000_000_000 + nanos)
}
