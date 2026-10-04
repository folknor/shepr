//! A transient action error and its expiry are one value.

use crate::limits::ENDPOINT_ERROR_TIMEOUT;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn nothing_shown_has_no_deadline_and_nothing_to_hide() {
        let mut error = TransientError::default();
        assert!(error.message().is_none());
        assert!(error.deadline().is_none());
        assert!(!error.tick(Instant::now()));
        assert!(!error.dismiss());
    }

    #[test]
    fn a_shown_error_expires_at_its_deadline_and_only_once() {
        let mut error = TransientError::default();
        let now = Instant::now();
        error.set("Copy failed", now);
        assert_eq!(error.message(), Some("Copy failed"));
        assert_eq!(error.deadline(), Some(now + ENDPOINT_ERROR_TIMEOUT));
        assert!(!error.tick(now));
        assert!(!error.tick(now + ENDPOINT_ERROR_TIMEOUT - Duration::from_millis(1)));
        assert_eq!(error.message(), Some("Copy failed"));
        assert!(error.tick(now + ENDPOINT_ERROR_TIMEOUT));
        assert!(error.message().is_none());
        assert!(error.deadline().is_none());
        assert!(!error.tick(now + ENDPOINT_ERROR_TIMEOUT));
    }

    #[test]
    fn setting_again_restarts_the_lifetime_even_for_the_same_message() {
        let mut error = TransientError::default();
        let first = Instant::now();
        let second = first + Duration::from_secs(1);
        error.set("Copy failed", first);
        error.set("Copy failed", second);
        assert_eq!(error.deadline(), Some(second + ENDPOINT_ERROR_TIMEOUT));
        assert!(!error.tick(first + ENDPOINT_ERROR_TIMEOUT));
        error.set("Paste failed", second);
        assert_eq!(error.message(), Some("Paste failed"));
    }

    #[test]
    fn dismiss_hides_the_error_before_its_deadline() {
        let mut error = TransientError::default();
        let now = Instant::now();
        error.set("Copy failed", now);
        assert!(error.dismiss());
        assert!(error.message().is_none());
        assert!(!error.dismiss());
        assert!(!error.tick(now + ENDPOINT_ERROR_TIMEOUT));
    }
}
