//! A transient action error and its expiry are one value.

/// Time an endpoint error stays visible without another input event.
///
/// The timeout leaves time to read a transient error before it clears.
const ENDPOINT_ERROR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Default)]
pub(in crate::shell) struct TransientError {
    current: Option<(String, std::time::Instant)>,
}

impl TransientError {
    pub(in crate::shell) fn message(&self) -> Option<&str> {
        self.current.as_ref().map(|(message, _)| message.as_str())
    }
    pub(in crate::shell) fn deadline(&self) -> Option<std::time::Instant> {
        self.current.as_ref().map(|(_, deadline)| *deadline)
    }
    /// Shows `message` with a fresh lifetime, even when it repeats the one
    /// already shown.
    pub(in crate::shell) fn set(&mut self, message: impl Into<String>, now: std::time::Instant) {
        self.current = Some((message.into(), now + ENDPOINT_ERROR_TIMEOUT));
    }
    pub(in crate::shell) fn dismiss(&mut self) -> bool {
        self.current.take().is_some()
    }
    pub(in crate::shell) fn tick(&mut self, now: std::time::Instant) -> bool {
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            self.dismiss()
        } else {
            false
        }
    }
}
