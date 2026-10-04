//! The loop's own timing: when it may render, when a held render is due, and
//! when a failed automatic workspace creation may retry, folded with the app's
//! and the shell cwd refresh's deadlines into the one next wake.

use std::time::Instant;

use crate::backoff::Backoff;

/// Minimum spacing between renders, matching a typical display refresh
/// cadence: rendering faster only produces frames no screen can show, while
/// output bursts coalesce into the next frame.
const MIN_RENDER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
/// First retry after automatic workspace creation fails, such as when the
/// configured shell stops resolving after server launch.
pub(super) const DEFAULT_WORKSPACE_RETRY_MIN: std::time::Duration =
    std::time::Duration::from_millis(250);
/// Cap on the doubling retry delay of automatic workspace creation, so it
/// still recovers soon after the shell or working directory becomes usable.
const DEFAULT_WORKSPACE_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// When the loop may render, and when a held render is due.
#[derive(Default)]
pub(super) struct RenderCadence {
    /// Last render-loop attempt, including a throttled hidden-only PTY skip.
    last_render_at: Option<Instant>,
    /// Last attempt that could update a connected presentation surface.
    last_presentation_at: Option<Instant>,
}

impl RenderCadence {
    pub(super) fn can_render(&self, now: Instant) -> bool {
        Self::interval_elapsed(self.last_render_at, now)
    }

    pub(super) fn can_present(&self, now: Instant) -> bool {
        Self::interval_elapsed(self.last_presentation_at, now)
    }

    /// Records a render attempt; `presentation` when it could update a
    /// connected presentation surface (not a hidden-only PTY skip).
    pub(super) fn record(&mut self, now: Instant, presentation: bool) {
        self.last_render_at = Some(now);
        if presentation {
            self.last_presentation_at = Some(now);
        }
    }

    /// `last_render_at + MIN_RENDER_INTERVAL` while a render is owed and that
    /// time is still ahead.
    pub(super) fn deadline(&self, now: Instant, render_owed: bool) -> Option<Instant> {
        if !render_owed {
            return None;
        }
        self.last_render_at
            .map(|last| last + MIN_RENDER_INTERVAL)
            .filter(|deadline| *deadline > now)
    }

    fn interval_elapsed(last: Option<Instant>, now: Instant) -> bool {
        last.is_none_or(|last| now.duration_since(last) >= MIN_RENDER_INTERVAL)
    }
}

const CREATION_BACKOFF: Backoff =
    Backoff::new(DEFAULT_WORKSPACE_RETRY_MIN, DEFAULT_WORKSPACE_RETRY_MAX);

/// Backoff of the automatic workspace after a failed creation.
#[derive(Default)]
pub(super) struct CreationRetry {
    retry_at: Option<Instant>,
    failures: u32,
}

impl CreationRetry {
    pub(super) fn may_attempt(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|retry_at| now >= retry_at)
    }

    pub(super) fn failed(&mut self, now: Instant) {
        self.retry_at = Some(now + CREATION_BACKOFF.delay_after(self.failures));
        self.failures = self.failures.saturating_add(1);
    }

    /// A workspace exists: the next failure starts from the minimum delay.
    pub(super) fn reset(&mut self) {
        self.retry_at = None;
        self.failures = 0;
    }

    /// A future retry instant; a past one waits for the next wake.
    pub(super) fn deadline(&self, now: Instant) -> Option<Instant> {
        self.retry_at.filter(|retry_at| *retry_at > now)
    }
}

#[derive(Default)]
pub(super) struct LoopSchedule {
    pub(super) cadence: RenderCadence,
    pub(super) creation: CreationRetry,
}

#[derive(Clone, Copy)]
pub(super) struct WakeInputs {
    /// A full projection or a render signal is waiting on the cadence.
    pub(super) render_owed: bool,
    pub(super) app: Option<Instant>,
    pub(super) shell_cwd: Option<Instant>,
}

impl LoopSchedule {
    /// The one deadline fold: the earliest of the app's deadline, the render
    /// cadence, the creation retry and the shell cwd refresh.
    pub(super) fn next_wake(&self, now: Instant, inputs: WakeInputs) -> Option<Instant> {
        [
            inputs.app,
            self.cadence.deadline(now, inputs.render_owed),
            self.creation.deadline(now),
            inputs.shell_cwd,
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn hidden_render_attempt_keeps_presentation_cadence_available() {
        let mut cadence = RenderCadence::default();
        let initial_presentation = Instant::now();
        cadence.record(initial_presentation, true);

        let hidden_attempt = initial_presentation + MIN_RENDER_INTERVAL;
        cadence.record(hidden_attempt, false);
        let foreground_echo = hidden_attempt + Duration::from_millis(1);

        assert!(!cadence.can_render(foreground_echo));
        assert!(cadence.can_present(foreground_echo));
    }

    #[test]
    fn creation_retry_backs_off_and_resets() {
        let mut retry = CreationRetry::default();
        let now = Instant::now();
        assert!(retry.may_attempt(now));

        retry.failed(now);
        assert!(!retry.may_attempt(now));
        assert_eq!(retry.deadline(now), Some(now + DEFAULT_WORKSPACE_RETRY_MIN));
        assert!(retry.may_attempt(now + DEFAULT_WORKSPACE_RETRY_MIN));

        let later = now + DEFAULT_WORKSPACE_RETRY_MIN;
        retry.failed(later);
        assert_eq!(
            retry.deadline(later),
            Some(later + CREATION_BACKOFF.delay_after(1))
        );

        retry.reset();
        assert!(retry.may_attempt(later));
        assert_eq!(retry.deadline(later), None);
        retry.failed(later);
        assert_eq!(
            retry.deadline(later),
            Some(later + DEFAULT_WORKSPACE_RETRY_MIN)
        );
    }

    #[test]
    fn a_past_creation_retry_does_not_wake_the_loop() {
        let mut retry = CreationRetry::default();
        let now = Instant::now();
        retry.failed(now);
        let after = now + DEFAULT_WORKSPACE_RETRY_MIN + Duration::from_millis(1);

        assert_eq!(retry.deadline(after), None);
        let schedule = LoopSchedule {
            creation: retry,
            ..LoopSchedule::default()
        };
        assert_eq!(
            schedule.next_wake(
                after,
                WakeInputs {
                    render_owed: false,
                    app: None,
                    shell_cwd: None,
                }
            ),
            None
        );
    }

    #[test]
    fn next_wake_takes_the_earliest_future_deadline() {
        let now = Instant::now();
        let mut schedule = LoopSchedule::default();
        schedule.cadence.record(now, true);
        schedule.creation.failed(now);

        let wake = |app, shell_cwd| {
            schedule.next_wake(
                now,
                WakeInputs {
                    render_owed: true,
                    app,
                    shell_cwd,
                },
            )
        };
        // The render cadence (16 ms) is earlier than the creation retry (250 ms).
        assert_eq!(wake(None, None), Some(now + MIN_RENDER_INTERVAL));
        let app = now + Duration::from_millis(5);
        assert_eq!(wake(Some(app), None), Some(app));
        let cwd = now + Duration::from_millis(2);
        assert_eq!(wake(Some(app), Some(cwd)), Some(cwd));
    }

    #[test]
    fn render_deadline_only_while_a_render_is_owed() {
        let now = Instant::now();
        let mut cadence = RenderCadence::default();
        assert_eq!(cadence.deadline(now, true), None);

        cadence.record(now, true);
        assert_eq!(cadence.deadline(now, false), None);
        assert_eq!(cadence.deadline(now, true), Some(now + MIN_RENDER_INTERVAL));
        assert_eq!(
            cadence.deadline(now + MIN_RENDER_INTERVAL, true),
            None,
            "a deadline that has arrived is not ahead"
        );
    }
}
