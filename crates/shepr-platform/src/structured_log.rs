//! Shared structured event fields, independent of log sink initialization.

// Export through this crate so macro consumers need no direct tracing dependency.
#[doc(hidden)]
pub use tracing as tracing_backend;

/// Emit an event with the common `event`, `subsystem` and `outcome` fields.
///
/// Event names are `subsystem.operation`: two snake_case Rust identifiers.
/// The operation describes the action; success, failure and finer results go
/// in `outcome`, never in the event name. Additional tracing fields and the
/// message follow the outcome, using tracing's ordinary syntax.
///
/// ```ignore
/// shepr_platform::structured_log!(
///     WARN, event = persist.save, outcome = "not_durable",
///     path = %path.display(), %error, "session saved but not confirmed durable"
/// );
/// ```
#[macro_export]
macro_rules! structured_log {
    ($level:ident, event = $subsystem:ident.$operation:ident, outcome = $outcome:expr, $($fields:tt)+) => {
        $crate::tracing_backend::event!(
            $crate::tracing_backend::Level::$level,
            event = concat!(stringify!($subsystem), ".", stringify!($operation)),
            subsystem = stringify!($subsystem),
            outcome = $outcome,
            $($fields)+
        )
    };
}
