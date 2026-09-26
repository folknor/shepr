//! Submission lifecycle shared by the PTY actor and its cancellation handle.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::pty::fd;

pub struct QueuedSubmission {
    pub completion: std::sync::mpsc::Receiver<std::io::Result<()>>,
    pub cancel: SubmissionCancel,
}

/// How far a submission had got when it was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionCancelOutcome {
    /// Nothing was written; the submission is dropped.
    Withdrawn,
    /// Some or all of the text reached the pane, but Enter will not be sent.
    TextUnsubmitted,
    /// Enter was already being written; the prompt will be submitted.
    AlreadySubmitting,
    /// The actor already finished it; its result is on the completion channel.
    Finished,
}

/// Withdraws a queued submission whose caller stopped waiting for it (an
/// `agent.prompt --wait --timeout` that reached its deadline). Without this
/// the actor would still type the prompt, and press Enter, whenever the pane
/// next reads input, long after the caller was told it timed out.
///
/// Cancellation never cuts a write in half: a bracketed paste or an escape
/// sequence truncated mid-way would leave the agent's input parser in a state
/// that swallows whatever the user types next. So text already being written
/// is finished, and only what has not started is dropped - in particular the
/// Enter, which is never sent once the submission is cancelled. The handle
/// only flips the shared state; the actor owns its write queue.
#[derive(Clone)]
pub struct SubmissionCancel {
    pub(crate) state: SharedSubmissionState,
    pub(crate) wake: Option<fd::WakeWriter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmissionState {
    Queued,
    CancelledBeforeStart,
    WritingText {
        started: bool,
        cancelled: bool,
        delay: Duration,
    },
    WaitingForEnter {
        deadline: Instant,
        text_written: bool,
        cancelled: bool,
    },
    Submitting {
        started: bool,
        text_written: bool,
        cancelled: bool,
    },
    Finished,
}

pub(crate) type SharedSubmissionState = Arc<Mutex<SubmissionState>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnterStart {
    NotReady,
    Cancelled,
    Empty,
    Started,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmissionPart {
    Text,
    Enter,
}

impl SubmissionState {
    pub(crate) fn shared() -> SharedSubmissionState {
        Arc::new(Mutex::new(Self::Queued))
    }

    pub(crate) fn start(&mut self, text_empty: bool, delay: Duration) -> bool {
        if *self != Self::Queued {
            return false;
        }
        *self = if text_empty {
            Self::WaitingForEnter {
                deadline: Instant::now() + delay,
                text_written: false,
                cancelled: false,
            }
        } else {
            Self::WritingText {
                started: false,
                cancelled: false,
                delay,
            }
        };
        true
    }

    pub(crate) fn cancel(&mut self) -> SubmissionCancelOutcome {
        match self {
            Self::Queued => {
                *self = Self::CancelledBeforeStart;
                SubmissionCancelOutcome::Withdrawn
            }
            Self::CancelledBeforeStart => SubmissionCancelOutcome::Withdrawn,
            Self::WritingText {
                started, cancelled, ..
            } => {
                *cancelled = true;
                if *started {
                    SubmissionCancelOutcome::TextUnsubmitted
                } else {
                    SubmissionCancelOutcome::Withdrawn
                }
            }
            Self::WaitingForEnter {
                text_written,
                cancelled,
                ..
            } => {
                *cancelled = true;
                if *text_written {
                    SubmissionCancelOutcome::TextUnsubmitted
                } else {
                    SubmissionCancelOutcome::Withdrawn
                }
            }
            Self::Submitting {
                started,
                text_written,
                cancelled,
            } => {
                if *started {
                    SubmissionCancelOutcome::AlreadySubmitting
                } else {
                    *cancelled = true;
                    if *text_written {
                        SubmissionCancelOutcome::TextUnsubmitted
                    } else {
                        SubmissionCancelOutcome::Withdrawn
                    }
                }
            }
            Self::Finished => SubmissionCancelOutcome::Finished,
        }
    }

    /// Called with the state lock held around the first write syscall, so a
    /// cancel either wins before this part starts or observes it as started.
    pub(crate) fn can_write_first_byte(&self, part: SubmissionPart) -> bool {
        matches!(
            (self, part),
            (
                Self::WritingText {
                    started: false,
                    cancelled: false,
                    ..
                },
                SubmissionPart::Text,
            ) | (
                Self::Submitting {
                    started: false,
                    cancelled: false,
                    ..
                },
                SubmissionPart::Enter,
            )
        )
    }

    pub(crate) fn first_byte_written(&mut self, part: SubmissionPart) {
        match (self, part) {
            (Self::WritingText { started, .. }, SubmissionPart::Text)
            | (Self::Submitting { started, .. }, SubmissionPart::Enter) => *started = true,
            _ => {}
        }
    }

    pub(crate) fn text_finished(&mut self, now: Instant) -> bool {
        let Self::WritingText {
            started,
            cancelled,
            delay,
        } = *self
        else {
            return false;
        };
        *self = Self::WaitingForEnter {
            deadline: now + delay,
            text_written: started,
            cancelled,
        };
        true
    }

    pub(crate) fn start_enter(&mut self, now: Instant, enter_empty: bool) -> EnterStart {
        match self {
            Self::WaitingForEnter {
                cancelled: true, ..
            } => EnterStart::Cancelled,
            Self::WaitingForEnter { deadline, .. } if now < *deadline => EnterStart::NotReady,
            Self::WaitingForEnter { .. } if enter_empty => {
                *self = Self::Finished;
                EnterStart::Empty
            }
            Self::WaitingForEnter { text_written, .. } => {
                *self = Self::Submitting {
                    started: false,
                    text_written: *text_written,
                    cancelled: false,
                };
                EnterStart::Started
            }
            _ => EnterStart::NotReady,
        }
    }

    pub(crate) fn enter_finished(&mut self) -> bool {
        if !matches!(self, Self::Submitting { started: true, .. }) {
            return false;
        }
        *self = Self::Finished;
        true
    }

    pub(crate) fn cancelled(&self) -> bool {
        match self {
            Self::CancelledBeforeStart => true,
            Self::WritingText { cancelled, .. }
            | Self::WaitingForEnter { cancelled, .. }
            | Self::Submitting { cancelled, .. } => *cancelled,
            _ => false,
        }
    }

    pub(crate) fn should_withdraw(&self) -> bool {
        matches!(
            self,
            Self::CancelledBeforeStart
                | Self::WritingText {
                    started: false,
                    cancelled: true,
                    ..
                }
                | Self::WaitingForEnter {
                    cancelled: true,
                    ..
                }
                | Self::Submitting {
                    started: false,
                    cancelled: true,
                    ..
                }
        )
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        match self {
            Self::WaitingForEnter { deadline, .. } => Some(*deadline),
            _ => None,
        }
    }

    pub(crate) fn finish(&mut self) {
        *self = Self::Finished;
    }
}

impl SubmissionCancel {
    /// A cancel handle for a submission nothing else tracks: cancelling it
    /// always answers `Finished`, sending the caller to the completion.
    #[cfg(test)]
    pub(crate) fn untracked() -> Self {
        Self {
            state: Arc::new(Mutex::new(SubmissionState::Finished)),
            wake: None,
        }
    }

    /// A cancel handle for a submission no actor ever picks up: cancelling it
    /// answers `Withdrawn`.
    #[cfg(test)]
    pub(crate) fn never_started() -> Self {
        Self {
            state: SubmissionState::shared(),
            wake: None,
        }
    }

    pub fn cancel(&self) -> SubmissionCancelOutcome {
        let outcome = lock_state(&self.state).cancel();
        // The actor may be parked on the submission's delay or on a PTY that
        // is not writable; wake it so it drops the submission now and moves
        // on to the input queued behind it.
        if matches!(
            outcome,
            SubmissionCancelOutcome::Withdrawn | SubmissionCancelOutcome::TextUnsubmitted
        ) && let Some(wake) = &self.wake
            && let Err(err) = wake.wake()
        {
            tracing::debug!(err = %err, "failed to wake PTY actor for a cancelled submission");
        }
        outcome
    }
}

pub(crate) fn lock_state(
    state: &Mutex<SubmissionState>,
) -> std::sync::MutexGuard<'_, SubmissionState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_before_first_text_byte_withdraws_the_whole_submission() {
        let mut state = SubmissionState::Queued;
        assert!(state.start(false, Duration::ZERO));

        assert_eq!(state.cancel(), SubmissionCancelOutcome::Withdrawn);
        assert!(!state.can_write_first_byte(SubmissionPart::Text));
        assert!(state.text_finished(Instant::now()));
        assert!(state.should_withdraw());
    }

    #[test]
    fn cancellation_after_text_starts_finishes_text_without_enter() {
        let mut state = SubmissionState::Queued;
        assert!(state.start(false, Duration::ZERO));
        assert!(state.can_write_first_byte(SubmissionPart::Text));
        state.first_byte_written(SubmissionPart::Text);

        assert_eq!(state.cancel(), SubmissionCancelOutcome::TextUnsubmitted);
        assert!(!state.should_withdraw());
        assert!(state.text_finished(Instant::now()));
        assert!(state.should_withdraw());
    }

    #[test]
    fn cancellation_before_enter_starts_withdraws_it_but_started_enter_finishes() {
        let mut state = SubmissionState::Queued;
        assert!(state.start(false, Duration::ZERO));
        state.first_byte_written(SubmissionPart::Text);
        assert!(state.text_finished(Instant::now()));
        assert_eq!(
            state.start_enter(Instant::now(), false),
            EnterStart::Started
        );
        assert_eq!(state.cancel(), SubmissionCancelOutcome::TextUnsubmitted);
        assert!(state.should_withdraw());

        let mut state = SubmissionState::Queued;
        assert!(state.start(false, Duration::ZERO));
        state.first_byte_written(SubmissionPart::Text);
        assert!(state.text_finished(Instant::now()));
        assert_eq!(
            state.start_enter(Instant::now(), false),
            EnterStart::Started
        );
        assert!(state.can_write_first_byte(SubmissionPart::Enter));
        state.first_byte_written(SubmissionPart::Enter);
        assert_eq!(state.cancel(), SubmissionCancelOutcome::AlreadySubmitting);
        assert!(state.enter_finished());
        assert_eq!(state.cancel(), SubmissionCancelOutcome::Finished);
    }
}
