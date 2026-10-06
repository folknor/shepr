//! Shared structured event fields, independent of log sink initialization.

// Export through this crate so macro consumers need no direct tracing dependency.
#[doc(hidden)]
pub use tracing as tracing_backend;

/// Declares [`Outcome`] from one table of variants and spellings, so its
/// spelling match and its `ALL` list cannot drift apart.
macro_rules! outcomes {
    ($($(#[$doc:meta])* $variant:ident => $spelling:literal),* $(,)?) => {
        /// What an operator-diagnostic event reports happened. The set is closed:
        /// the `structured_log!` macro accepts only these, so a new spelling is a
        /// new variant here and not a free-text string at a call site. Each
        /// variant is one meaning; a call site that needs a finer distinction
        /// names it in the event or in a field, not in a synonym of an existing
        /// outcome.
        ///
        /// Failure is only ever [`Outcome::Error`], and success only
        /// [`Outcome::Ok`]: the cause of a failure goes in an `error` field.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Outcome {
            $($(#[$doc])* $variant),*
        }

        impl Outcome {
            /// Every outcome, so tests can hold the spellings unique and snake_case.
            pub const ALL: &'static [Outcome] = &[$(Self::$variant),*];

            /// The spelling the `outcome` field carries.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $spelling),*
                }
            }
        }
    };
}

outcomes! {
    /// An operation or phase began (also: a request was sent or dispatched).
    Started => "started",
    /// The operation succeeded.
    Ok => "ok",
    /// A running thing or attempt finished, whatever its result, and the
    /// event records that rather than a verdict.
    Ended => "ended",
    /// A loop, worker or listener stopped serving.
    Stopped => "stopped",
    /// Waiting on something outside the operation: a stage not finished, a
    /// retry scheduled by someone else, an operator action.
    Pending => "pending",
    /// A request was taken up; its result is reported separately.
    Accepted => "accepted",
    /// A transient failure; the operation will be attempted again.
    Retry => "retry",
    /// A previously failing thing works again.
    Recovered => "recovered",
    /// The work was given up on and dropped.
    Abandoned => "abandoned",
    /// Nothing was done because nothing was needed or it was disabled.
    Skipped => "skipped",
    /// The state already was what was asked for, so nothing changed.
    Unchanged => "unchanged",
    /// A value, record or ownership was altered, repaired or withdrawn.
    Changed => "changed",
    /// A deadline or hold ran out without the awaited thing happening.
    Expired => "expired",
    /// A bounded wait ended without an answer.
    Timeout => "timeout",
    /// A connection or endpoint became usable.
    Connected => "connected",
    /// A peer or channel went away without a clean detach.
    Disconnected => "disconnected",
    /// A client left on purpose.
    Detached => "detached",
    /// The operation failed; the cause is the `error` field.
    Error => "error",
    /// Policy or authority declined the request.
    Refused => "refused",
    /// The input is malformed or contradicts its contract.
    Invalid => "invalid",
    /// The input exceeds a size bound.
    Oversized => "oversized",
    /// A budget, limit or counter ran out.
    Exhausted => "exhausted",
    /// The same thing arrived or was registered twice.
    Duplicate => "duplicate",
    /// Something required is absent.
    Missing => "missing",
    /// A capability or resource could not be had right now.
    Unavailable => "unavailable",
    /// The resource is held by another party.
    Busy => "busy",
    /// The input belongs to a superseded generation and was dropped.
    Stale => "stale",
    /// A message or connection was discarded.
    Dropped => "dropped",
    /// A thread or task panicked.
    Panicked => "panicked",
    /// Shared state is unusable because a panic left it poisoned.
    Poisoned => "poisoned",
    /// Something outside the expected contract happened that is not a failure.
    Unexpected => "unexpected",
    /// Only part of the work took effect.
    Partial => "partial",
    /// A save landed but its durability was not confirmed.
    NotDurable => "not_durable",
    /// Saves are held because the backup could not be taken.
    BlockedOnBackup => "blocked_on_backup",
    /// Saves are frozen for a host shutdown.
    Frozen => "frozen",
    /// An endpoint failed in a way only the operator can resolve.
    NeedsAttention => "needs_attention",
    /// A lock was taken.
    Acquired => "acquired",
    /// A lock or hold was let go.
    Released => "released",
    /// A degraded alternative was used in place of the first choice.
    Fallback => "fallback",
}

/// Emit an event with the common `event`, `subsystem` and `outcome` fields.
///
/// Event names are `subsystem.operation`: two snake_case Rust identifiers, and
/// `scripts/check_structured_logs.py` holds the subsystem to its closed list.
/// The operation describes the action; success, failure and finer results go
/// in `outcome`, never in the event name. The outcome is an [`Outcome`] variant
/// written bare (`outcome = Error`), or any expression of type [`Outcome`] in
/// parentheses (`outcome = (saved.outcome())`); the compiler refuses anything
/// else. Additional tracing fields and the message follow the outcome, using
/// tracing's ordinary syntax. A failure's cause is the field `error`, never
/// `err`, `reason` or `failure`; the same script refuses those keys.
///
/// ```ignore
/// shepr_platform::structured_log!(
///     WARN, event = persist.save, outcome = NotDurable,
///     path = %path.display(), error = %error, "session saved but not confirmed durable"
/// );
/// ```
#[macro_export]
macro_rules! structured_log {
    ($level:ident, event = $subsystem:ident.$operation:ident, outcome = $outcome:ident, $($fields:tt)+) => {
        $crate::structured_log!(
            $level, event = $subsystem.$operation, outcome = ($crate::Outcome::$outcome),
            $($fields)+
        )
    };
    ($level:ident, event = $subsystem:ident.$operation:ident, outcome = ($outcome:expr), $($fields:tt)+) => {
        $crate::tracing_backend::event!(
            $crate::tracing_backend::Level::$level,
            event = concat!(stringify!($subsystem), ".", stringify!($operation)),
            subsystem = stringify!($subsystem),
            outcome = $crate::Outcome::as_str($outcome),
            $($fields)+
        )
    };
}

#[cfg(test)]
mod tests {
    use super::Outcome;
    use std::collections::HashSet;

    #[test]
    fn outcome_spellings_are_unique_snake_case() {
        let mut seen = HashSet::new();
        for outcome in Outcome::ALL {
            let text = outcome.as_str();
            assert!(
                !text.is_empty()
                    && text
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte == b'_'),
                "{text} is not snake_case"
            );
            assert!(seen.insert(text), "{text} is spelled twice");
        }
    }

    #[test]
    fn the_macro_takes_a_bare_variant_and_a_typed_expression() {
        crate::structured_log!(DEBUG, event = logging.install, outcome = Ok, "bare variant");
        let outcome = Outcome::Error;
        crate::structured_log!(
            DEBUG,
            event = logging.install,
            outcome = (outcome),
            "expression"
        );
    }
}
