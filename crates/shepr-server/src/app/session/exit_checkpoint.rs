use std::time::Instant;

use shepr_mux::persist::CapturedLayout;

use super::{CheckpointGeneration, checkpoint_retry_delay};
use crate::limits::CHECKPOINT_MAX_FAILURES;

/// Generations are issued 1, 2, 3, ... per held exit. `through` is the newest
/// generation released: a held exit with generation <= `through` may be
/// removed. A released generation is never held again, whatever follows.
pub(super) enum PaneExitCheckpoint {
    /// Nothing is held and there is no preserved layout.
    Idle {
        through: Option<CheckpointGeneration>,
    },
    /// Exits wait for generation `generation`'s checkpoint. Generations up to
    /// `through` are already released. `None` means none have been released.
    /// No layout is preserved.
    Requested {
        through: Option<CheckpointGeneration>,
        generation: CheckpointGeneration,
        /// Consecutive failed checkpoint saves of `generation`'s epoch.
        failures: u8,
        /// A failed attempt's retry; `None` means start at once.
        retry_at: Option<Instant>,
    },
    /// A checkpoint is durable, no session mutation has been observed since
    /// its capture, and its layout is preserved. Generations up to
    /// `generation` (the newest issued when it landed) are released.
    Saved {
        generation: CheckpointGeneration,
        /// The layout the checkpoint made durable, kept so the final save can
        /// rewrite it (with fresh cwds, `CapturedLayout::recapture`)
        /// instead of the layout after the exited panes left.
        layout: Box<CapturedLayout>,
    },
    /// Checkpoints failed `CHECKPOINT_MAX_FAILURES` times in a row, or the
    /// persister stopped accepting saves for this boot. Every generation ever
    /// issued is released (`through`), and no new exit is held until any save
    /// succeeds.
    Abandoned {
        through: Option<CheckpointGeneration>,
    },
}

impl PaneExitCheckpoint {
    pub(super) fn new() -> Self {
        Self::Idle { through: None }
    }

    /// The newest generation issued so far, or `None` before the first exit.
    fn issued(&self) -> Option<CheckpointGeneration> {
        match self {
            Self::Idle { through } | Self::Abandoned { through } => *through,
            Self::Requested { generation, .. } | Self::Saved { generation, .. } => {
                Some(*generation)
            }
        }
    }

    pub(super) fn is_released(&self, generation: CheckpointGeneration) -> bool {
        match self {
            Self::Idle { through }
            | Self::Requested { through, .. }
            | Self::Abandoned { through } => generation.is_released_by(*through),
            Self::Saved {
                generation: through,
                ..
            } => generation.is_released_by(Some(*through)),
        }
    }

    pub(super) fn is_requested(&self) -> bool {
        matches!(self, Self::Requested { .. })
    }

    pub(super) fn pending_generation(&self) -> Option<CheckpointGeneration> {
        if let Self::Requested { generation, .. } = self {
            Some(*generation)
        } else {
            None
        }
    }

    pub(super) fn retry_at(&self) -> Option<Instant> {
        if let Self::Requested { retry_at, .. } = self {
            *retry_at
        } else {
            None
        }
    }

    pub(super) fn preserved(&self) -> Option<&CapturedLayout> {
        if let Self::Saved { layout, .. } = self {
            Some(layout)
        } else {
            None
        }
    }

    /// Whether an exit arriving now must wait for a checkpoint: Idle,
    /// Requested and a Saved whose session has since changed
    /// (`session_dirty`) hold; a Saved with an unchanged session and
    /// Abandoned do not. `session_dirty` is AppState's current pending signal;
    /// the app checks it directly when deciding whether a preserved layout is
    /// still authoritative.
    pub(super) fn would_hold(&self, session_dirty: bool) -> bool {
        match self {
            Self::Idle { .. } | Self::Requested { .. } => true,
            Self::Saved { .. } => session_dirty,
            Self::Abandoned { .. } => false,
        }
    }

    /// Holds an exit: returns its generation, or `None` when it does not
    /// hold. A request while one is pending issues the next generation and
    /// keeps the pending failures and retry.
    pub(super) fn request(&mut self, session_dirty: bool) -> Option<CheckpointGeneration> {
        if !self.would_hold(session_dirty) {
            return None;
        }
        let through = self.issued();
        let next = through.map_or_else(CheckpointGeneration::first, CheckpointGeneration::next);
        if let Self::Requested { generation, .. } = self {
            *generation = next;
        } else {
            *self = Self::Requested {
                through,
                generation: next,
                failures: 0,
                retry_at: None,
            };
        }
        Some(next)
    }

    /// A checkpoint save of `saved_generation` succeeded. `layout` is its
    /// preserved layout, `None` when it could not be paired with identities
    /// or a mutation has been seen since the capture. A layout releases every
    /// held exit, newer ones included: a held exit's pane stays in the layout
    /// until its replay, and a pane created after the capture is a mutation
    /// that voided the layout. Without a layout, only the captured generation
    /// is released, and a newer one waits for its own checkpoint.
    pub(super) fn saved(
        &mut self,
        saved_generation: CheckpointGeneration,
        layout: Option<Box<CapturedLayout>>,
    ) {
        let Self::Requested {
            through,
            generation,
            failures,
            retry_at,
        } = self
        else {
            return;
        };
        if saved_generation > *generation {
            return;
        }
        if let Some(layout) = layout {
            *self = Self::Saved {
                generation: *generation,
                layout,
            };
        } else if saved_generation == *generation {
            *self = Self::Idle {
                through: Some(*generation),
            };
        } else {
            *through =
                Some((*through).map_or(saved_generation, |through| through.max(saved_generation)));
            *failures = 0;
            *retry_at = None;
        }
    }

    /// A checkpoint save of `failed_generation` failed. Returns whether this
    /// abandoned the checkpoint. A failure of the pending generation counts
    /// toward abandonment; a failure of an older capture only re-arms the
    /// retry, without charging the newer epoch.
    pub(super) fn failed(&mut self, failed_generation: CheckpointGeneration, now: Instant) -> bool {
        let Self::Requested {
            generation,
            failures,
            retry_at,
            ..
        } = self
        else {
            return false;
        };
        if failed_generation > *generation {
            return false;
        }
        let delay = checkpoint_retry_delay(*failures);
        if failed_generation == *generation {
            *failures = failures.saturating_add(1);
            if *failures >= CHECKPOINT_MAX_FAILURES {
                *self = Self::Abandoned {
                    through: Some(*generation),
                };
                return true;
            }
        }
        *retry_at = Some(now + delay);
        false
    }

    /// Any save succeeded: an abandonment ends (its generations stay
    /// released), and a pending checkpoint's failure count restarts.
    pub(super) fn save_succeeded(&mut self) {
        match self {
            Self::Abandoned { through } => *self = Self::Idle { through: *through },
            Self::Requested { failures, .. } => *failures = 0,
            Self::Idle { .. } | Self::Saved { .. } => {}
        }
    }

    /// The preserved layout stopped being authoritative (a mutation, or a
    /// save of the live layout, replaced it). Its generations stay released.
    pub(super) fn discard_layout(&mut self) {
        if let Self::Saved { generation, .. } = self {
            *self = Self::Idle {
                through: Some(*generation),
            };
        }
    }

    /// A host-shutdown checkpoint was requested: a pending retry is dropped,
    /// so the combined save starts at once.
    pub(super) fn expedite_retry(&mut self) {
        if let Self::Requested { retry_at, .. } = self {
            *retry_at = None;
        }
    }

    /// A host-shutdown freeze releases every held exit.
    pub(super) fn release_for_freeze(&mut self) {
        if let Self::Requested { generation, .. } = self {
            *self = Self::Idle {
                through: Some(*generation),
            };
        }
    }

    /// Releases every held exit when the persister cannot accept another
    /// checkpoint during this boot.
    pub(super) fn abandon(&mut self) {
        let through = self.issued();
        *self = Self::Abandoned { through };
    }

    #[cfg(test)]
    pub(super) fn preserved_mut(&mut self) -> Option<&mut CapturedLayout> {
        if let Self::Saved { layout, .. } = self {
            Some(layout)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::SESSION_SAVE_RETRY_MIN;

    fn layout() -> Box<CapturedLayout> {
        Box::new(CapturedLayout::new(
            shepr_mux::persist::schema::SessionSnapshot {
                version: shepr_mux::persist::schema::SNAPSHOT_VERSION,
                host_theme: Default::default(),
                workspaces: vec![],
                active: None,
            },
            std::collections::HashMap::new(),
        ))
    }
    fn generation(value: u64) -> CheckpointGeneration {
        CheckpointGeneration(value)
    }
    fn requested() -> PaneExitCheckpoint {
        let mut exit = PaneExitCheckpoint::new();
        assert_eq!(exit.request(false), Some(generation(1)));
        exit
    }
    fn saved() -> PaneExitCheckpoint {
        let mut exit = requested();
        exit.saved(generation(1), Some(layout()));
        exit
    }
    #[test]
    fn a_request_on_an_idle_machine_issues_the_next_generation() {
        let exit = requested();
        assert_eq!(exit.pending_generation(), Some(generation(1)));
        assert!(!exit.is_released(generation(1)));
        assert_eq!(exit.retry_at(), None);
    }
    #[test]
    fn a_request_while_requested_keeps_failures_and_retry() {
        let mut exit = requested();
        let now = Instant::now();
        exit.failed(generation(1), now);
        assert_eq!(exit.request(false), Some(generation(2)));
        assert!(matches!(
            exit,
            PaneExitCheckpoint::Requested { failures: 1, .. }
        ));
        assert_eq!(exit.retry_at(), Some(now + SESSION_SAVE_RETRY_MIN));
    }
    #[test]
    fn exits_after_a_saved_layout_with_an_unchanged_session_hold_nothing() {
        let mut exit = saved();
        assert!(!exit.would_hold(false));
        assert_eq!(exit.request(false), None);
        assert!(exit.preserved().is_some());
    }
    #[test]
    fn a_dirty_session_after_a_saved_layout_requests_again() {
        let mut exit = saved();
        assert!(exit.would_hold(true));
        assert_eq!(exit.request(true), Some(generation(2)));
        assert!(exit.is_released(generation(1)));
        assert!(exit.preserved().is_none());
    }
    #[test]
    fn saving_the_newest_generation_preserves_the_layout() {
        let exit = saved();
        assert!(exit.is_released(generation(1)));
        assert!(exit.preserved().is_some());
        assert!(!exit.is_requested());
    }
    #[test]
    fn saving_without_a_layout_releases_without_preserving() {
        let mut exit = requested();
        exit.saved(generation(1), None);
        assert!(exit.is_released(generation(1)));
        assert!(exit.preserved().is_none());
        assert_eq!(exit.request(false), Some(generation(2)));
    }
    #[test]
    fn an_older_generation_saved_with_a_layout_releases_every_held_exit() {
        let mut exit = requested();
        exit.request(false);
        exit.saved(generation(1), Some(layout()));
        assert!(matches!(
            exit,
            PaneExitCheckpoint::Saved {
                generation: CheckpointGeneration(2),
                ..
            }
        ));
        assert!(exit.is_released(generation(1)));
        assert!(exit.is_released(generation(2)));
        assert!(!exit.would_hold(false));
        assert_eq!(exit.request(false), None);
    }
    #[test]
    fn an_older_generation_saved_without_a_layout_keeps_the_newer_exit_held() {
        let mut exit = requested();
        exit.request(false);
        exit.saved(generation(1), None);
        assert!(exit.is_released(generation(1)));
        assert!(!exit.is_released(generation(2)));
        assert_eq!(exit.request(false), Some(generation(3)));
    }
    #[test]
    fn a_mutation_discards_the_layout_and_keeps_its_generation_released() {
        let mut exit = saved();
        exit.discard_layout();
        assert!(exit.is_released(generation(1)));
        assert!(exit.preserved().is_none());
        assert_eq!(exit.request(false), Some(generation(2)));
    }
    #[test]
    fn three_failures_of_the_newest_generation_abandon_it_and_release_every_generation() {
        let mut exit = requested();
        let now = Instant::now();
        for _ in 1..CHECKPOINT_MAX_FAILURES {
            assert!(!exit.failed(generation(1), now));
            assert!(!exit.is_released(generation(1)));
        }
        assert!(exit.failed(generation(1), now));
        assert!(exit.is_released(generation(1)));
        assert_eq!(exit.request(true), None);
        assert!(!exit.would_hold(true));
    }
    #[test]
    fn a_failure_of_an_older_generation_re_arms_the_retry_without_counting() {
        let mut exit = requested();
        let now = Instant::now();
        exit.failed(generation(1), now);
        exit.request(false);
        assert!(!exit.failed(generation(1), now));
        assert_eq!(exit.retry_at(), Some(now + SESSION_SAVE_RETRY_MIN * 2));
        assert!(matches!(
            exit,
            PaneExitCheckpoint::Requested { failures: 1, .. }
        ));
    }
    #[test]
    fn any_successful_save_ends_the_abandonment_without_unreleasing_abandoned_generations() {
        let mut exit = requested();
        for _ in 0..CHECKPOINT_MAX_FAILURES {
            exit.failed(generation(1), Instant::now());
        }
        exit.save_succeeded();
        assert!(exit.is_released(generation(1)));
        assert_eq!(exit.request(false), Some(generation(2)));
        exit.failed(generation(2), Instant::now());
        exit.save_succeeded();
        assert!(matches!(
            exit,
            PaneExitCheckpoint::Requested { failures: 0, .. }
        ));
    }
    #[test]
    fn expediting_clears_the_retry_and_keeps_the_failures() {
        let mut exit = requested();
        exit.failed(generation(1), Instant::now());
        exit.expedite_retry();
        assert_eq!(exit.retry_at(), None);
        assert!(matches!(
            exit,
            PaneExitCheckpoint::Requested { failures: 1, .. }
        ));
    }
    #[test]
    fn freezing_releases_the_held_generation() {
        let mut exit = requested();
        exit.release_for_freeze();
        assert!(exit.is_released(generation(1)));
        assert_eq!(exit.request(false), Some(generation(2)));
    }
    #[test]
    fn issued_generations_never_repeat_through_any_transition() {
        let mut exit = requested();
        exit.saved(generation(1), Some(layout()));
        assert_eq!(exit.request(true), Some(generation(2)));
        exit.saved(generation(2), None);
        assert_eq!(exit.request(false), Some(generation(3)));
        exit.release_for_freeze();
        assert_eq!(exit.request(false), Some(generation(4)));
        for _ in 0..CHECKPOINT_MAX_FAILURES {
            exit.failed(generation(4), Instant::now());
        }
        exit.save_succeeded();
        assert_eq!(exit.request(false), Some(generation(5)));
        exit.saved(generation(5), Some(layout()));
        exit.discard_layout();
        assert_eq!(exit.request(false), Some(generation(6)));
        for value in 1..=5 {
            assert!(exit.is_released(generation(value)));
        }
    }
}
