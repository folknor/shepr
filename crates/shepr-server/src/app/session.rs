//! When the session is saved, and what a save's outcome means for the loop.
//!
//! Saving itself belongs to the session persister
//! (`shepr_mux::persist::SessionPersister`), which owns the data directory
//! lease, the writer and the pane history carried between saves on a thread
//! of its own. This side decides when to save (debounced autosaves, pane-exit
//! and host-shutdown checkpoints, retries), captures what to save on the
//! event loop, where only the cheap part happens (the structural snapshot, a
//! handle to each pane's terminal and a probe of each shell's cwd), and hands
//! the result to the persister.
//!
//! The loop learns that a save finished from the persister's completion
//! signal ([`SessionSaver::save_finished`]), not by polling: it waits on the
//! signal and reaps the save when it fires. While a save is in flight no
//! save deadline is reported, since nothing can start before the save ends,
//! and its end wakes the loop to reconsider them.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{App, Backoff};
use crate::limits::{CHECKPOINT_MAX_FAILURES, CHECKPOINT_RETRY_MAX_DELAY, SESSION_SAVE_RETRY_MIN};

mod autosave;
mod exit_checkpoint;
mod host_checkpoint;
use autosave::Autosave;
use exit_checkpoint::{PaneExitCheckpoint, PreservedLayout};
use host_checkpoint::HostShutdownCheckpoint;

/// Identity of a pane-exit checkpoint request, minted by `PaneExitCheckpoint`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CheckpointGeneration(u64);

impl CheckpointGeneration {
    fn first() -> Self {
        Self(1)
    }

    fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    fn is_released_by(self, through: Option<Self>) -> bool {
        through.is_some_and(|through| self <= through)
    }
}

/// A save the persister is running, and what it was asked to make durable.
struct InFlightSave {
    pending: shepr_mux::persist::PendingSave,
    kind: SaveKind,
}

enum SaveKind {
    Autosave,
    Checkpoint(CheckpointTicket),
}

/// What a checkpoint save was asked to make durable. Cancelling the host
/// request voids only the host half.
struct CheckpointTicket {
    exit: Option<ExitTicket>,
    /// Whether this save answers the host-shutdown request.
    host: bool,
}

/// The held pane-exit generation a checkpoint save answers. A mutation
/// observed after the capture voids the layout but not the generation: the
/// exit is still released when the save lands.
struct ExitTicket {
    generation: CheckpointGeneration,
    /// The captured layout; `None` when it could not be paired with terminal
    /// identities, or a session mutation was observed after the capture.
    layout: Option<Box<PreservedLayout>>,
}

/// The save `start_background_session_save` should start now.
enum NextSave {
    Autosave,
    Checkpoint {
        exit_generation: Option<CheckpointGeneration>,
        host: bool,
    },
}

pub(crate) struct SessionSaver {
    policy: SavePolicy,
    autosave: Autosave,
    exit: PaneExitCheckpoint,
    host: HostShutdownCheckpoint,
    /// At most one save is in flight: a due save waits for it, so every
    /// capture reaches the persister after the one before it finished.
    in_flight: Option<InFlightSave>,
    persister: shepr_mux::persist::SessionPersister,
    /// Fired by the persister each time a submitted save ends.
    save_finished: Arc<tokio::sync::Notify>,
}

#[derive(Clone, Copy)]
enum SaveMode {
    Never,
    Persisting,
    Stopped,
}

/// The one runtime decision that admits a session save. The persisted mode is
/// retained across a host-shutdown freeze and restored only by `thaw`.
#[derive(Clone, Copy)]
enum SavePolicy {
    Never,
    Persisting,
    /// The persister refused a job it can never run; no save starts again
    /// this boot.
    Stopped,
    Frozen {
        resume_to: SaveMode,
    },
}

impl SavePolicy {
    fn new(persists: bool) -> Self {
        if persists {
            Self::Persisting
        } else {
            Self::Never
        }
    }

    fn allows_saves(self) -> bool {
        matches!(self, Self::Persisting)
    }

    /// Whether a host-shutdown checkpoint request is taken: a stopped saver
    /// takes it only to fail it at once, so the lifecycle stops waiting.
    /// Never-persisting and frozen savers ignore it.
    fn takes_host_checkpoint(self) -> bool {
        matches!(self, Self::Persisting | Self::Stopped)
    }

    fn is_stopped(self) -> bool {
        matches!(
            self,
            Self::Stopped
                | Self::Frozen {
                    resume_to: SaveMode::Stopped
                }
        )
    }

    fn freeze(&mut self) {
        let resume_to = match self {
            Self::Never => SaveMode::Never,
            Self::Persisting => SaveMode::Persisting,
            Self::Stopped => SaveMode::Stopped,
            Self::Frozen { .. } => return,
        };
        *self = Self::Frozen { resume_to };
    }

    fn thaw(&mut self) {
        let resume_to = match self {
            Self::Frozen { resume_to } => *resume_to,
            Self::Never | Self::Persisting | Self::Stopped => return,
        };
        *self = match resume_to {
            SaveMode::Never => Self::Never,
            SaveMode::Persisting => Self::Persisting,
            SaveMode::Stopped => Self::Stopped,
        };
    }

    fn stop(&mut self) {
        if let Self::Frozen { resume_to } = self {
            *resume_to = SaveMode::Stopped;
        } else {
            *self = Self::Stopped;
        }
    }
}

/// Retry delay of a failed pane-exit or host-shutdown checkpoint after
/// `failures_before` earlier failures of the same checkpoint: doubling from
/// `SESSION_SAVE_RETRY_MIN`, capped at `CHECKPOINT_RETRY_MAX_DELAY`. Total for
/// every `u8`, so raising `CHECKPOINT_MAX_FAILURES` cannot make it overflow.
fn checkpoint_retry_delay(failures_before: u8) -> Duration {
    Backoff::new(SESSION_SAVE_RETRY_MIN, CHECKPOINT_RETRY_MAX_DELAY)
        .delay_after(u32::from(failures_before))
}

impl SessionSaver {
    /// `save_finished` is the signal `persister` was built with.
    pub(crate) fn new(
        persister: shepr_mux::persist::SessionPersister,
        save_finished: Arc<tokio::sync::Notify>,
        persists: bool,
    ) -> Self {
        Self {
            policy: SavePolicy::new(persists),
            autosave: Autosave::new(),
            exit: PaneExitCheckpoint::new(),
            host: HostShutdownCheckpoint::new(),
            in_flight: None,
            persister,
            save_finished,
        }
    }

    /// The signal the persister fires when a save ends. The headless loop
    /// waits on it; a firing with nothing to reap is harmless.
    pub(crate) fn save_finished(&self) -> &tokio::sync::Notify {
        &self.save_finished
    }

    /// Whether nothing may start now whatever is requested or due: a save is
    /// in flight (its end fires [`Self::save_finished`], which wakes the
    /// loop), or the host checkpoint finished unsaved and no pane exit is
    /// held (the lifecycle freezes saves once it takes that result), or this
    /// boot's persister has stopped.
    fn blocked(&self) -> bool {
        !self.policy.allows_saves()
            || self.in_flight.is_some()
            || (self.host.finished_unsaved() && !self.exit.is_requested())
    }

    /// Whether a checkpoint, rather than an autosave, is the next save.
    fn checkpoint_requested(&self) -> bool {
        self.exit.is_requested() || self.host.is_requested()
    }

    /// When the loop should next try to start a save. A requested checkpoint
    /// reports the later of the two machines' retries, and none when neither
    /// carries one: it starts at once, from the call that requested it or
    /// from the reap of the save before it. A persister that refused work
    /// stops reporting deadlines for this boot.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        if self.blocked() {
            None
        } else if self.checkpoint_requested() {
            self.exit
                .retry_at()
                .into_iter()
                .chain(self.host.retry_at())
                .max()
        } else {
            self.autosave.deadline()
        }
    }

    pub(crate) fn is_due(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|d| now >= d)
    }

    /// The save to start now, by the same rule as [`Self::deadline`].
    fn next_save(&self, now: Instant) -> Option<NextSave> {
        if self.blocked() {
            None
        } else if self.checkpoint_requested() {
            if self
                .exit
                .retry_at()
                .into_iter()
                .chain(self.host.retry_at())
                .any(|d| now < d)
            {
                return None;
            }
            Some(NextSave::Checkpoint {
                exit_generation: self.exit.pending_generation(),
                host: self.host.is_requested(),
            })
        } else {
            self.autosave.is_due(now).then_some(NextSave::Autosave)
        }
    }

    /// Applies one consumed session mutation: the autosave is due after the
    /// debounce, a preserved pane-exit layout stops being authoritative, and
    /// the layout captured by a checkpoint in flight is voided, so a capture
    /// that predates the mutation is never installed. This is scheduling
    /// bookkeeping; layout authority is checked against AppState's current
    /// dirty bit at each use, so this cache is not a second mutation source.
    fn note_mutation(&mut self, now: Instant) {
        if !self.policy.allows_saves() {
            return;
        }
        self.autosave.schedule(now);
        self.exit.discard_layout();
        if let Some(InFlightSave {
            kind: SaveKind::Checkpoint(ticket),
            ..
        }) = &mut self.in_flight
            && let Some(exit) = &mut ticket.exit
        {
            exit.layout = None;
        }
    }

    /// Requests the host-shutdown checkpoint; returns whether it was newly
    /// requested. A pane-exit retry is expedited, so the combined save starts
    /// at once instead of waiting for it.
    fn request_host_checkpoint(&mut self) -> bool {
        let requested = self.host.request();
        if requested {
            if self.policy.is_stopped() {
                self.host.fail_permanently();
            } else {
                self.exit.expedite_retry();
            }
        }
        requested
    }

    fn stop_persistence(&mut self) {
        self.policy.stop();
        self.autosave.clear();
        self.exit.abandon();
        self.host.fail_permanently();
    }

    /// Drops the host-shutdown request or its unclaimed result, and voids the
    /// host half of a checkpoint in flight, so a later request is never
    /// answered by a save captured before it.
    fn cancel_host_checkpoint(&mut self) {
        self.host.cancel();
        if let Some(InFlightSave {
            kind: SaveKind::Checkpoint(ticket),
            ..
        }) = &mut self.in_flight
        {
            ticket.host = false;
        }
    }

    /// Stops scheduled saves for a host shutdown. Exits held for a pane-exit
    /// checkpoint are released: the host-shutdown checkpoint already captured
    /// the layout they were held in (or ran out of retries). A save already in
    /// flight is not cancelled; it captured the pre-freeze session, so what
    /// it writes is still a pre-freeze state. Nothing starts after the freeze.
    pub(crate) fn freeze(&mut self) {
        self.policy.freeze();
        self.autosave.clear();
        self.exit.release_for_freeze();
    }

    pub(crate) fn thaw(&mut self) {
        self.policy.thaw();
    }
}

impl App {
    /// Freezes the saver and marks the app suspended as one lifecycle change.
    pub(crate) fn freeze_session_saves(&mut self) {
        self.policy = super::AppPolicy::Suspended;
        self.session_saver.freeze();
    }

    /// Restores the app's pre-freeze policy and thaws the saver together.
    pub(crate) fn thaw_session_saves(&mut self, policy: super::AppPolicy) {
        self.session_saver.thaw();
        self.policy = policy;
    }

    /// The current AppState dirty bit is the authority for a saved exit layout;
    /// saver mutation notifications only schedule writes and invalidate caches.
    pub(super) fn preserves_pane_exit_checkpoint(&self) -> bool {
        self.session_saver.exit.preserved().is_some() && !self.state.session_dirty
    }

    // A missing identity must never replace the protected layout with the
    // post-exit layout. Keep the durable checkpoint if refreshing it fails.
    fn capture_final_session_save_job(&self) -> Option<shepr_mux::persist::PersistJob> {
        let Some(layout) = self
            .session_saver
            .exit
            .preserved()
            .filter(|_| !self.state.session_dirty)
        else {
            return Some(self.capture_session_save_job().0);
        };
        let job = self.capture_save_job_from_preserved_layout(layout);
        if job.is_none() {
            tracing::warn!(
                "could not pair fresh pane history with the saved pane-exit layout; keeping the durable checkpoint"
            );
        }
        job
    }

    /// Consumes the pure state mutation signal and applies persistence effects
    /// once for this loop pass. AppState mutations and App-owned mutations use
    /// the same flag, so a handler cannot schedule the same change twice.
    pub(crate) fn sync_session_save_schedule(&mut self) {
        if self.state.session_dirty {
            self.state.session_dirty = false;
            if self.session_saver.policy.allows_saves() {
                self.session_saver.note_mutation(self.clock.now);
            }
        }
    }

    /// Records the save in flight if it has finished, without waiting.
    /// Returns whether it reaped one; the loop then reconsiders starting the
    /// next save, since none could start while this one ran.
    pub(crate) fn reap_finished_session_save(&mut self) -> bool {
        let Some(result) = self
            .session_saver
            .in_flight
            .as_ref()
            .and_then(|save| save.pending.try_finish())
        else {
            return false;
        };
        if let Some(save) = self.session_saver.in_flight.take() {
            self.finish_session_save(save.kind, result);
        }
        true
    }

    /// Whether session saves have stopped for the rest of this boot: the
    /// persister refused a save it can never run, so layout changes from now
    /// on are not restored by the next server start.
    pub(crate) fn session_saves_stopped(&self) -> bool {
        self.session_saver.policy.is_stopped()
    }

    /// The one place a save's outcome is applied to the autosave backoff and
    /// both checkpoint machines.
    fn finish_session_save(
        &mut self,
        kind: SaveKind,
        result: Result<(), shepr_mux::persist::SaveError>,
    ) {
        let now = self.clock.now;
        match result {
            Ok(()) => {
                if let Some(failures) = self.session_saver.autosave.record_success() {
                    tracing::info!(failures, "session save recovered after failures");
                }
                self.session_saver.exit.save_succeeded();
                match kind {
                    SaveKind::Autosave => self.session_saver.exit.discard_layout(),
                    SaveKind::Checkpoint(ticket) => {
                        if let Some(exit) = ticket.exit {
                            self.session_saver.exit.saved(
                                exit.generation,
                                // This catches a mutation not yet consumed by
                                // the loop's save-scheduling pass.
                                exit.layout.filter(|_| !self.state.session_dirty),
                            );
                        } else {
                            self.session_saver.exit.discard_layout();
                        }
                        if ticket.host {
                            self.session_saver.host.saved();
                        }
                    }
                }
                // The next checkpoint capture supersedes a scheduled autosave.
                if self.session_saver.exit.is_requested() {
                    self.session_saver.autosave.clear();
                }
            }
            Err(error) if !error.is_retryable() => {
                tracing::error!(
                    error = %error,
                    "session persister cannot accept further saves; disabling session persistence for this boot"
                );
                if !self.session_saver.policy.is_stopped() {
                    // Every client's snapshot carries the stop, so the user
                    // learns that later layout changes will not be restored.
                    self.state.mark_shell_projection_dirty();
                }
                self.session_saver.stop_persistence();
            }
            Err(error) => {
                // A retryable write failure re-arms the normal retry; a
                // checkpoint's own retry is its machine's, below. (A refusal
                // or abandonment took the branch above.)
                let (failures, delay) = self.session_saver.autosave.record_failure(now);
                tracing::warn!(error = %error, failures, retry_ms = delay.as_millis(), "session save failed");
                if let SaveKind::Checkpoint(ticket) = kind {
                    if let Some(exit) = ticket.exit
                        && self.session_saver.exit.failed(exit.generation, now)
                    {
                        tracing::warn!(
                            failures = CHECKPOINT_MAX_FAILURES,
                            "pane exit checkpoint failed repeatedly; removing exited panes without persisting their exit"
                        );
                    }
                    if ticket.host && self.session_saver.host.failed(now) {
                        tracing::warn!("host shutdown checkpoint failed repeatedly");
                    }
                }
            }
        }
    }

    /// Runs on the event loop, so it takes only what must be read here: the
    /// structural snapshot, a handle to each pane's terminal and a probe of
    /// each shell's cwd. No terminal lock is taken and no /proc file is read;
    /// turning history into its saved form and reading the cwds are the
    /// persister's work.
    fn capture_session_save_job(
        &self,
    ) -> (
        shepr_mux::persist::PersistJob,
        HashMap<shepr_mux::persist::snapshot::SavedPaneRef, shepr_protocol::TerminalId>,
    ) {
        shepr_mux::persist::capture_job(
            &self.state.workspaces,
            &self.state.terminals,
            &self.terminal_runtimes,
            self.paths.fallback_cwd(),
            self.state.bookmark_index(),
            self.state.host_terminal_theme,
            self.persist_pane_history,
        )
    }

    /// The layout a pane-exit checkpoint preserves from its capture, or
    /// `None` unless `job` is a save and every pane of every workspace has a
    /// terminal identity to refresh its history and cwd from at the final
    /// save.
    fn capture_preserved_layout(
        job: &shepr_mux::persist::PersistJob,
        terminal_ids: HashMap<
            shepr_mux::persist::snapshot::SavedPaneRef,
            shepr_protocol::TerminalId,
        >,
    ) -> Option<PreservedLayout> {
        let shepr_mux::persist::PersistJob::Save(bundle) = job else {
            return None;
        };
        if terminal_ids.len()
            != bundle
                .snapshot
                .workspaces
                .iter()
                .map(|workspace| workspace.panes.len())
                .sum::<usize>()
        {
            return None;
        }
        Some(PreservedLayout {
            snapshot: bundle.snapshot.clone(),
            terminal_ids,
        })
    }

    fn capture_save_job_from_preserved_layout(
        &self,
        layout: &PreservedLayout,
    ) -> Option<shepr_mux::persist::PersistJob> {
        let cwds = shepr_mux::persist::snapshot::capture_pending_cwds_for_snapshot(
            &layout.snapshot,
            &layout.terminal_ids,
            &self.terminal_runtimes,
        )?;
        let history = if self.persist_pane_history {
            Some(
                shepr_mux::persist::snapshot::capture_pending_history_for_snapshot(
                    &layout.snapshot,
                    &layout.terminal_ids,
                    &self.terminal_runtimes,
                )?,
            )
        } else {
            None
        };
        Some(shepr_mux::persist::PersistJob::Save(
            shepr_mux::persist::SessionBundle {
                snapshot: layout.snapshot.clone(),
                cwds,
                history,
            },
        ))
    }

    pub(crate) fn start_background_session_save(&mut self) {
        if !self.session_saver.policy.allows_saves() {
            self.session_saver.autosave.clear();
            return;
        }
        self.reap_finished_session_save();
        match self.session_saver.next_save(self.clock.now) {
            None => {}
            Some(NextSave::Checkpoint {
                exit_generation,
                host,
            }) => {
                if std::mem::take(&mut self.state.session_dirty) {
                    self.session_saver.note_mutation(self.clock.now);
                }
                self.session_saver.autosave.clear();
                let (job, terminal_ids) = self.capture_session_save_job();
                let ticket = CheckpointTicket {
                    exit: exit_generation.map(|generation| ExitTicket {
                        generation,
                        layout: Self::capture_preserved_layout(&job, terminal_ids).map(Box::new),
                    }),
                    host,
                };
                self.spawn_session_save(job, SaveKind::Checkpoint(ticket));
            }
            Some(NextSave::Autosave) => {
                self.session_saver.autosave.clear();
                let (job, _) = self.capture_session_save_job();
                self.spawn_session_save(job, SaveKind::Autosave);
            }
        }
    }

    fn spawn_session_save(&mut self, job: shepr_mux::persist::PersistJob, kind: SaveKind) {
        let pending = self
            .session_saver
            .persister
            .submit(job, self.clock.wall_now);
        self.session_saver.in_flight = Some(InFlightSave { pending, kind });
    }

    /// Starts a new checkpoint for a pane exit and returns its generation, or
    /// `None` when the exit is already settled. Each held exit gets a
    /// generation so a save captured before that exit cannot release it when
    /// the save later finishes, and a later change to the session cannot hold
    /// it again.
    pub(crate) fn request_pane_exit_checkpoint(&mut self) -> Option<CheckpointGeneration> {
        if !self.session_saver.policy.allows_saves() {
            return None;
        }
        let generation = self.session_saver.exit.request(self.state.session_dirty)?;
        self.start_background_session_save();
        Some(generation)
    }

    /// Whether a held exit's generation is released: a checkpoint holding its
    /// pane is durable, or checkpoints were abandoned or frozen.
    pub(crate) fn pane_exit_checkpoint_generation_settled(
        &self,
        generation: CheckpointGeneration,
    ) -> bool {
        !self.session_saver.policy.allows_saves() || self.session_saver.exit.is_released(generation)
    }

    pub(crate) fn request_host_shutdown_checkpoint(&mut self) {
        if !self.session_saver.policy.takes_host_checkpoint()
            || !self.session_saver.request_host_checkpoint()
        {
            return;
        }
        self.start_background_session_save();
    }

    pub(crate) fn host_shutdown_checkpoint_result_ready(&self) -> bool {
        self.session_saver.host.is_finished()
    }

    pub(crate) fn take_host_shutdown_checkpoint_result(&mut self) -> Option<bool> {
        self.session_saver.host.take_result()
    }

    pub(crate) fn cancel_host_shutdown_checkpoint(&mut self) {
        self.session_saver.cancel_host_checkpoint();
    }

    pub(crate) fn finish_checkpointed_pane_exit(&mut self) {
        self.finish_checkpointed_pane_exit_after_event(false);
    }

    pub(crate) fn finish_checkpointed_pane_exit_after_event(&mut self, session_was_dirty: bool) {
        if self.session_saver.exit.preserved().is_some() {
            if session_was_dirty {
                // Preserve mutations that arrived after the checkpoint capture.
                // They need a current-state save even though the exit itself
                // was safe to replay from its generation's checkpoint.
                self.session_saver.exit.discard_layout();
            } else {
                // Removing the already-checkpointed pane is not a new durable
                // mutation; the later save can update the layout after debounce.
                // The event path sampled this flag before applying the removal,
                // so this only clears the removal's own dirty mark. Other
                // event-loop mutations set it before this call and take the
                // branch above. Captures and save completions also consult the
                // flag directly before accepting a preserved layout. The
                // session dirty bit is sufficient here because the event loop
                // samples it immediately before applying one event and no
                // other mutation can interleave; a second counter would not
                // change which layout this synchronous path accepts.
                self.state.session_dirty = false;
            }
            self.session_saver.autosave.schedule(self.clock.now);
        }
    }

    fn submit_final_session_save(&mut self) -> Option<shepr_mux::persist::PendingSave> {
        if !self.session_saver.policy.allows_saves() {
            self.session_saver.autosave.clear();
            return None;
        }

        let Some(job) = self.capture_final_session_save_job() else {
            self.session_saver.autosave.clear();
            return None;
        };
        Some(
            self.session_saver
                .persister
                .submit(job, self.clock.wall_now),
        )
    }

    fn finish_final_session_save(
        &mut self,
        result: Result<(), shepr_mux::persist::SaveError>,
    ) -> bool {
        let saved = result.is_ok();
        self.finish_session_save(SaveKind::Autosave, result);
        if saved {
            self.session_saver.autosave.clear();
        }
        saved
    }

    pub(crate) async fn save_session_before_teardown_async(&mut self) {
        if let Some(save) = self.session_saver.in_flight.take() {
            let result = wait_off_the_runtime(save.pending).await;
            self.finish_session_save(save.kind, result);
        }

        let Some(pending) = self.submit_final_session_save() else {
            return;
        };
        let result = wait_off_the_runtime(pending).await;
        self.finish_final_session_save(result);
    }

    /// Ends persistence for this server: the save still in flight finishes
    /// (its failure is logged like any other save's; the retry it schedules
    /// is moot, the deadline is cleared below), then the persister releases
    /// the data directory lease. A `server stop` waits for that release.
    pub(crate) fn retire_session_writer(&mut self) {
        if let Some(save) = self.session_saver.in_flight.take() {
            let result = save.pending.wait();
            self.finish_session_save(save.kind, result);
        }
        self.session_saver.autosave.clear();
        self.session_saver.persister.retire();
    }
}

/// Waits for a persister result on a blocking thread, so the async runtime
/// keeps serving while the save finishes.
async fn wait_off_the_runtime(
    pending: shepr_mux::persist::PendingSave,
) -> Result<(), shepr_mux::persist::SaveError> {
    match tokio::task::spawn_blocking(move || pending.wait()).await {
        Ok(result) => result,
        Err(_) => Err(shepr_mux::persist::SaveError::Abandoned),
    }
}

#[cfg(test)]
use shepr_mux::events::AppEvent;

#[cfg(test)]
impl SessionSaver {
    /// The autosave deadline itself, which [`Self::deadline`] does not report
    /// while a save is in flight or a checkpoint is requested.
    pub(crate) fn autosave_deadline(&self) -> Option<Instant> {
        self.autosave.deadline()
    }

    pub(crate) fn set_autosave_deadline(&mut self, deadline: Option<Instant>) {
        self.autosave.set_deadline(deadline);
    }

    /// Admits saves without a running persister, for tests of the saver's
    /// scheduling alone.
    pub(crate) fn admit_saves_for_test(&mut self) {
        self.policy = SavePolicy::Persisting;
    }

    /// Whether a save is in flight.
    pub(crate) fn save_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Stands in for an autosave the persister is still running; it
    /// finishes when the test completes the returned handle.
    pub(crate) fn hold_test_save_in_flight(&mut self) -> shepr_mux::persist::SaveCompletion {
        self.hold_test_kind(SaveKind::Autosave)
    }

    /// Stands in for a pane-exit checkpoint of `generation` with no
    /// preserved layout, finishing when the test completes the handle.
    pub(crate) fn hold_test_checkpoint_in_flight(
        &mut self,
        generation: CheckpointGeneration,
    ) -> shepr_mux::persist::SaveCompletion {
        self.hold_test_kind(SaveKind::Checkpoint(CheckpointTicket {
            exit: Some(ExitTicket {
                generation,
                layout: None,
            }),
            host: false,
        }))
    }

    fn hold_test_kind(&mut self, kind: SaveKind) -> shepr_mux::persist::SaveCompletion {
        let (completion, pending) = shepr_mux::persist::PendingSave::channel();
        self.in_flight = Some(InFlightSave { pending, kind });
        completion
    }

    /// Private: `PreservedLayout` is only visible inside `session`.
    fn preserved_layout_mut(&mut self) -> Option<&mut PreservedLayout> {
        self.exit.preserved_mut()
    }
}

#[cfg(test)]
impl App {
    /// Turns a test app into a persisting one, as production boots: the
    /// lease-only persister gives way to a threaded one on the same data
    /// directory, and the policy becomes Production. Tests set up their state
    /// first, so that setup schedules no saves.
    pub(crate) fn persist_for_test(&mut self) {
        self.session_saver.persister.retire();
        let lease = shepr_mux::persist::DataDirLease::acquire(self.paths.data_dir())
            .expect("the test data directory lease is free");
        self.session_saver.persister = shepr_mux::persist::SessionPersister::spawn(
            lease,
            shepr_mux::persist::SessionBackupPolicy::NoBackupNeeded,
            shepr_mux::persist::HistoryCarry::default(),
            Arc::clone(&self.session_saver.save_finished),
        );
        self.session_saver.policy = SavePolicy::Persisting;
        self.policy = super::AppPolicy::Production;
    }

    /// Blocks until the save in flight, if any, has finished, records its
    /// outcome and returns whether it succeeded; `None` when nothing was in
    /// flight.
    fn wait_for_session_save_with_outcome(&mut self) -> Option<bool> {
        let save = self.session_saver.in_flight.take()?;
        let result = save.pending.wait();
        let saved = result.is_ok();
        self.finish_session_save(save.kind, result);
        Some(saved)
    }

    /// Blocks until the save in flight, if any, has finished, and records
    /// its outcome.
    pub(super) fn wait_for_session_save(&mut self) {
        self.wait_for_session_save_with_outcome();
    }

    pub(crate) fn save_session_now(&mut self) -> bool {
        self.wait_for_session_save();

        if !self.session_saver.policy.allows_saves() {
            self.session_saver.autosave.clear();
            return !self.session_saver.policy.is_stopped();
        }

        self.session_saver
            .set_autosave_deadline(Some(self.clock.now));
        self.start_background_session_save();
        self.wait_for_session_save_with_outcome() == Some(true)
    }

    /// Save the live pane histories while runtimes still exist, keeping the
    /// directory claim until their processes have finished tearing down.
    pub(crate) fn save_session_before_teardown(&mut self) {
        self.wait_for_session_save();
        let Some(pending) = self.submit_final_session_save() else {
            return;
        };
        self.finish_final_session_save(pending.wait());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(
            &shepr_config::ServerConfig::default(),
            super::super::AppPolicy::Suspended,
        )
    }

    /// An app whose saver admits saves, for tests of the saver's scheduling
    /// alone: no persister runs, so only what the saver decides is observed.
    fn saving_test_app() -> App {
        let mut app = test_app();
        app.session_saver.admit_saves_for_test();
        app
    }

    /// A production-policy app with one workspace of two panes, returning the
    /// pane that will exit.
    fn two_pane_app(name: &str) -> (App, shepr_core::layout::PaneId, shepr_core::layout::PaneId) {
        use crate::test_support::WorkspaceFixture as _;
        let mut app = test_app();
        app.persist_for_test();
        let mut workspace = shepr_mux::workspace::Workspace::test_new(name);
        let exiting = workspace.root_pane();
        let staying = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        app.state.test_set_workspaces(vec![workspace]);
        app.state.set_bookmark_index(Some(0));
        app.state.ensure_test_terminals();
        app.insert_idle_test_runtime(exiting);
        app.insert_idle_test_runtime(staying);
        (app, exiting, staying)
    }

    /// A signalled exit of the pane, as its runtime reports it.
    fn interrupted_exit(app: &App, pane_id: shepr_core::layout::PaneId) -> AppEvent {
        app.from_pane_runtime(
            pane_id,
            AppEvent::PaneDied {
                pane_id,
                exit_reason: shepr_platform::ChildExitReason::Interrupted,
                ended_at: std::time::Instant::now(),
            },
        )
    }

    fn saved_pane_counts(app: &App) -> Vec<usize> {
        let saved = std::fs::read_to_string(
            app.paths
                .data_dir()
                .join(shepr_mux::persist::SessionWriter::SESSION_FILE_NAME),
        )
        .expect("read the session file");
        shepr_mux::persist::snapshot::parse_session_file(&saved)
            .expect("parse the session file")
            .snapshot
            .workspaces
            .iter()
            .map(|workspace| workspace.panes.len())
            .collect()
    }

    #[test]
    fn a_held_pane_exit_settles_on_its_checkpoint_although_the_session_changed_meanwhile() {
        let (mut app, exiting, _) = two_pane_app("held");

        let generation = app
            .prepare_pane_exit(
                exiting,
                shepr_platform::ChildExitReason::Interrupted,
                std::time::Instant::now(),
            )
            .held_generation()
            .expect("a signalled exit is held for a checkpoint");
        assert!(app.session_saver.save_in_flight());
        // A change lands while the checkpoint is being written, and the loop
        // turns it into a scheduled save.
        app.state.mark_session_dirty();
        app.sync_session_save_schedule();
        app.wait_for_session_save();

        assert!(app.pane_exit_checkpoint_generation_settled(generation));
        assert!(
            !app.session_saver.exit.is_requested(),
            "the exit asks for no second checkpoint"
        );
        assert!(!app.preserves_pane_exit_checkpoint());
        assert_eq!(saved_pane_counts(&app), vec![2]);
    }

    #[test]
    fn a_save_captured_before_a_pane_exit_does_not_settle_it() {
        let (mut app, exiting, _) = two_pane_app("earlier-save");
        let earlier = app.session_saver.hold_test_save_in_flight();

        let generation = app
            .prepare_pane_exit(
                exiting,
                shepr_platform::ChildExitReason::Interrupted,
                std::time::Instant::now(),
            )
            .held_generation()
            .expect("a signalled exit is held for a checkpoint");
        earlier.complete(Ok(()));
        assert!(app.reap_finished_session_save());
        assert!(
            !app.pane_exit_checkpoint_generation_settled(generation),
            "the autosave in flight was captured before the exit"
        );

        app.start_background_session_save();
        app.wait_for_session_save();
        assert!(app.pane_exit_checkpoint_generation_settled(generation));
    }

    #[test]
    fn exits_after_a_pane_exit_checkpoint_keep_its_layout() {
        let (app, exiting, staying) = two_pane_app("burst");
        let mut server = crate::server::headless::tests::test_headless_server();
        server.app = app;

        server.handle_test_runtime_exit_and_replay(interrupted_exit(&server.app, exiting));
        assert_eq!(saved_pane_counts(&server.app), vec![2]);
        assert_eq!(
            server.app.prepare_pane_exit(
                staying,
                shepr_platform::ChildExitReason::Interrupted,
                std::time::Instant::now(),
            ),
            crate::app::PreparedPaneExit::Settled,
            "the checkpoint on disk already holds the second pane"
        );
    }

    #[tokio::test]
    async fn the_final_save_rewrites_the_checkpoint_layout_instead_of_skipping_it() {
        let (app, exiting, _) = two_pane_app("final");
        let mut server = crate::server::headless::tests::test_headless_server();
        server.app = app;
        server.handle_test_runtime_exit_and_replay(interrupted_exit(&server.app, exiting));
        assert_eq!(server.app.state.workspaces[0].panes().len(), 1);
        // A final save that skipped would leave no session file behind.
        std::fs::remove_file(
            server
                .app
                .paths
                .data_dir()
                .join(shepr_mux::persist::SessionWriter::SESSION_FILE_NAME),
        )
        .expect("remove the checkpoint");

        server.app.save_session_before_teardown_async().await;

        assert_eq!(
            saved_pane_counts(&server.app),
            vec![2],
            "the final save keeps the layout the checkpoint saved"
        );
        server.app.retire_session_writer();
    }

    #[tokio::test]
    async fn a_final_save_with_missing_checkpoint_identities_keeps_the_durable_layout() {
        let (app, exiting, _) = two_pane_app("identities");
        let mut server = crate::server::headless::tests::test_headless_server();
        server.app = app;
        server.handle_test_runtime_exit_and_replay(interrupted_exit(&server.app, exiting));
        server
            .app
            .session_saver
            .preserved_layout_mut()
            .expect("saved checkpoint")
            .terminal_ids
            .clear();

        server.app.save_session_before_teardown_async().await;

        assert_eq!(saved_pane_counts(&server.app), vec![2]);
        assert!(server.app.preserves_pane_exit_checkpoint());
        server.app.retire_session_writer();
    }

    fn exit_kind(generation: CheckpointGeneration) -> SaveKind {
        SaveKind::Checkpoint(CheckpointTicket {
            exit: Some(ExitTicket {
                generation,
                layout: None,
            }),
            host: false,
        })
    }

    fn disk_full() -> Result<(), shepr_mux::persist::SaveError> {
        Err(shepr_mux::persist::SaveError::Io(std::io::Error::other(
            "disk full",
        )))
    }

    #[test]
    fn repeated_pane_exit_checkpoint_failures_release_the_held_exit() {
        let mut app = test_app();
        app.persist_for_test();
        let generation = app.session_saver.exit.request(true).expect("held");
        for _ in 1..CHECKPOINT_MAX_FAILURES {
            app.finish_session_save(exit_kind(generation), disk_full());
            assert!(!app.pane_exit_checkpoint_generation_settled(generation));
            assert!(app.session_saver.exit.retry_at().is_some());
        }
        app.finish_session_save(exit_kind(generation), disk_full());
        assert!(app.pane_exit_checkpoint_generation_settled(generation));
        assert_eq!(app.request_pane_exit_checkpoint(), None);
        app.finish_session_save(SaveKind::Autosave, Ok(()));
        assert!(app.pane_exit_checkpoint_generation_settled(generation));
        let next = app
            .request_pane_exit_checkpoint()
            .expect("a save that succeeds again restores pre-exit checkpoints");
        assert!(!app.pane_exit_checkpoint_generation_settled(next));
        assert!(app.pane_exit_checkpoint_generation_settled(generation));
        app.wait_for_session_save();
    }

    #[test]
    fn a_refused_save_stops_persistence_and_tells_every_client() {
        let mut app = test_app();
        app.persist_for_test();
        let generation = app.session_saver.exit.request(true).expect("held");
        let projection_before = app.state.shell_projection_revision;
        assert!(!app.session_saves_stopped());

        app.finish_session_save(
            exit_kind(generation),
            Err(shepr_mux::persist::SaveError::Refused(
                shepr_mux::persist::SaveRefusal::StoppedAfterPanic,
            )),
        );

        assert!(app.session_saves_stopped());
        assert_ne!(app.state.shell_projection_revision, projection_before);
        assert!(app.pane_exit_checkpoint_generation_settled(generation));
        assert_eq!(app.request_pane_exit_checkpoint(), None);
        assert_eq!(app.session_saver.deadline(), None);
    }

    #[test]
    fn a_requested_checkpoint_is_chosen_over_a_due_autosave() {
        let mut app = saving_test_app();
        let now = app.clock.now;
        let saver = &mut app.session_saver;
        saver.set_autosave_deadline(Some(now));
        let generation = saver.exit.request(false);
        assert!(
            matches!(saver.next_save(now), Some(NextSave::Checkpoint { exit_generation, host: false }) if exit_generation == generation)
        );
    }

    #[test]
    fn nothing_starts_while_a_save_is_in_flight_and_no_deadline_is_reported() {
        let mut app = test_app();
        let now = app.clock.now;
        let saver = &mut app.session_saver;
        saver.set_autosave_deadline(Some(now));
        saver.exit.request(false);
        saver.host.request();
        let completion = saver.hold_test_save_in_flight();
        assert!(saver.next_save(now).is_none());
        assert_eq!(saver.deadline(), None);
        completion.complete(Ok(()));
        app.reap_finished_session_save();
    }

    #[test]
    fn a_checkpoint_waits_for_the_later_of_the_two_retry_deadlines() {
        let mut app = saving_test_app();
        let now = app.clock.now;
        let generation = app.session_saver.exit.request(false).expect("exit");
        app.session_saver.host.request();
        app.session_saver.host.failed(now);
        app.finish_session_save(
            SaveKind::Checkpoint(CheckpointTicket {
                exit: Some(ExitTicket {
                    generation,
                    layout: None,
                }),
                host: true,
            }),
            disk_full(),
        );
        let later = now + SESSION_SAVE_RETRY_MIN * 2;
        assert_eq!(
            app.session_saver.exit.retry_at(),
            Some(now + SESSION_SAVE_RETRY_MIN)
        );
        assert_eq!(app.session_saver.host.retry_at(), Some(later));
        assert_eq!(app.session_saver.deadline(), Some(later));
        assert!(
            app.session_saver
                .next_save(later - Duration::from_nanos(1))
                .is_none()
        );
        assert!(
            matches!(app.session_saver.next_save(later), Some(NextSave::Checkpoint { exit_generation: Some(g), host: true }) if g == generation)
        );
    }

    #[test]
    fn a_host_request_expedites_a_pending_exit_retry() {
        let mut app = saving_test_app();
        let now = app.clock.now;
        let generation = app.session_saver.exit.request(false).expect("exit");
        app.session_saver.exit.failed(generation, now);
        assert!(app.session_saver.next_save(now).is_none());
        assert!(app.session_saver.request_host_checkpoint());
        assert!(
            matches!(app.session_saver.next_save(now), Some(NextSave::Checkpoint { exit_generation: Some(g), host: true }) if g == generation)
        );
    }

    #[test]
    fn a_failed_host_checkpoint_reports_no_deadline_and_starts_nothing() {
        let mut app = test_app();
        app.session_saver.host.request();
        for _ in 0..CHECKPOINT_MAX_FAILURES {
            app.finish_session_save(
                SaveKind::Checkpoint(CheckpointTicket {
                    exit: None,
                    host: true,
                }),
                disk_full(),
            );
        }
        assert!(app.session_saver.autosave_deadline().is_some());
        assert_eq!(app.session_saver.deadline(), None);
        assert!(
            app.session_saver
                .next_save(app.clock.now + SESSION_SAVE_RETRY_MIN * 100)
                .is_none()
        );
    }

    #[test]
    fn cancelling_the_host_checkpoint_voids_the_host_half_of_the_save_in_flight() {
        let mut app = test_app();
        app.session_saver.request_host_checkpoint();
        let completion = app
            .session_saver
            .hold_test_kind(SaveKind::Checkpoint(CheckpointTicket {
                exit: None,
                host: true,
            }));
        app.session_saver.cancel_host_checkpoint();
        assert!(app.session_saver.request_host_checkpoint());
        completion.complete(Ok(()));
        app.reap_finished_session_save();
        assert!(!app.session_saver.host.is_finished());
        assert!(app.session_saver.host.is_requested());
    }

    #[test]
    fn a_mutation_after_the_capture_voids_the_in_flight_layout() {
        let mut app = saving_test_app();
        let generation = app.session_saver.exit.request(false).expect("held");
        let layout = Box::new(PreservedLayout {
            snapshot: shepr_mux::persist::SessionSnapshot {
                version: shepr_mux::persist::snapshot::SNAPSHOT_VERSION,
                host_theme: Default::default(),
                workspaces: vec![],
                active: None,
            },
            terminal_ids: HashMap::new(),
        });
        let completion = app
            .session_saver
            .hold_test_kind(SaveKind::Checkpoint(CheckpointTicket {
                exit: Some(ExitTicket {
                    generation,
                    layout: Some(layout),
                }),
                host: false,
            }));
        assert!(matches!(
            &app.session_saver.in_flight,
            Some(InFlightSave {
                kind: SaveKind::Checkpoint(CheckpointTicket {
                    exit: Some(ExitTicket {
                        layout: Some(_),
                        ..
                    }),
                    ..
                }),
                ..
            })
        ));
        app.session_saver.note_mutation(app.clock.now);
        assert!(matches!(
            &app.session_saver.in_flight,
            Some(InFlightSave {
                kind: SaveKind::Checkpoint(CheckpointTicket {
                    exit: Some(ExitTicket { layout: None, .. }),
                    ..
                }),
                ..
            })
        ));
        completion.complete(Ok(()));
        assert!(app.reap_finished_session_save());
        assert!(app.session_saver.exit.is_released(generation));
        assert!(!app.preserves_pane_exit_checkpoint());
    }

    #[test]
    fn freezing_clears_the_autosave_and_releases_a_held_exit() {
        let mut app = test_app();
        let generation = app.session_saver.exit.request(false).expect("held");
        app.session_saver.autosave.schedule(app.clock.now);
        app.session_saver.freeze();
        assert_eq!(app.session_saver.autosave_deadline(), None);
        assert!(app.session_saver.exit.is_released(generation));
        assert!(app.session_saver.next_save(app.clock.now).is_none());
    }

    #[test]
    fn a_failed_autosave_does_not_delay_a_requested_checkpoint() {
        let (mut app, _, _) = two_pane_app("autosave-failure");
        let completion = app.session_saver.hold_test_save_in_flight();
        let generation = app.request_pane_exit_checkpoint().expect("held");
        completion.complete(disk_full());
        assert!(app.reap_finished_session_save());
        app.start_background_session_save();
        assert!(app.session_saver.save_in_flight());
        assert_eq!(app.session_saver.exit.retry_at(), None);
        app.wait_for_session_save();
        assert!(app.pane_exit_checkpoint_generation_settled(generation));
    }

    #[test]
    fn a_failed_pane_exit_checkpoint_retries_on_its_own_backoff() {
        let (mut app, _, _) = two_pane_app("own-backoff");
        for _ in 0..6 {
            app.finish_session_save(SaveKind::Autosave, disk_full());
        }
        // Clear the earlier retry, so the next failure exposes its full delay.
        app.session_saver.autosave.clear();
        let generation = app.session_saver.exit.request(true).expect("held");
        app.finish_session_save(exit_kind(generation), disk_full());
        assert_eq!(
            app.session_saver.exit.retry_at(),
            Some(app.clock.now + SESSION_SAVE_RETRY_MIN)
        );
        assert_eq!(
            app.session_saver.autosave_deadline(),
            Some(app.clock.now + SESSION_SAVE_RETRY_MIN * 64)
        );
    }

    #[test]
    fn a_host_request_starts_at_once_during_a_pane_exit_retry() {
        let (mut app, _, _) = two_pane_app("host-expedites");
        let generation = app.session_saver.exit.request(true).expect("held");
        app.finish_session_save(exit_kind(generation), disk_full());
        assert!(app.session_saver.exit.retry_at().is_some());
        app.request_host_shutdown_checkpoint();
        assert!(app.session_saver.save_in_flight());
        app.wait_for_session_save();
        assert!(app.pane_exit_checkpoint_generation_settled(generation));
        assert_eq!(app.take_host_shutdown_checkpoint_result(), Some(true));
    }

    #[test]
    fn a_host_shutdown_checkpoint_saves_the_live_layout_and_finishes() {
        let (mut app, _, _) = two_pane_app("host-live");
        app.request_host_shutdown_checkpoint();
        app.wait_for_session_save();
        assert!(app.host_shutdown_checkpoint_result_ready());
        assert_eq!(app.take_host_shutdown_checkpoint_result(), Some(true));
        assert_eq!(app.take_host_shutdown_checkpoint_result(), None);
        assert_eq!(saved_pane_counts(&app), vec![2]);
    }

    #[test]
    fn a_host_checkpoint_supersedes_a_preserved_pane_exit_layout() {
        let (app, exiting, _) = two_pane_app("host-supersedes");
        let mut server = crate::server::headless::tests::test_headless_server();
        server.app = app;
        server.handle_test_runtime_exit_and_replay(interrupted_exit(&server.app, exiting));
        assert!(server.app.preserves_pane_exit_checkpoint());
        server.app.request_host_shutdown_checkpoint();
        server.app.wait_for_session_save();
        assert_eq!(saved_pane_counts(&server.app), vec![1]);
        assert!(!server.app.preserves_pane_exit_checkpoint());
        assert_eq!(
            server.app.take_host_shutdown_checkpoint_result(),
            Some(true)
        );
    }

    #[test]
    fn a_mutation_pending_when_a_checkpoint_starts_discards_the_preserved_layout() {
        let (app, exiting, _) = two_pane_app("pending-mutation");
        let mut server = crate::server::headless::tests::test_headless_server();
        server.app = app;
        server.handle_test_runtime_exit_and_replay(interrupted_exit(&server.app, exiting));
        assert!(server.app.preserves_pane_exit_checkpoint());
        server.app.state.mark_session_dirty();
        server.app.request_host_shutdown_checkpoint();
        assert!(server.app.session_saver.save_in_flight());
        assert!(!server.app.preserves_pane_exit_checkpoint());
        assert!(server.app.session_saver.exit.preserved().is_none());
        server.app.wait_for_session_save();
    }

    #[test]
    fn a_cancelled_host_checkpoint_in_flight_does_not_answer_a_new_request() {
        let (mut app, _, _) = two_pane_app("host-cancel");
        app.request_host_shutdown_checkpoint();
        // Hold delivery of the real writer's result across cancellation and
        // re-request, regardless of how quickly the filesystem finishes.
        let (completion, pending) = shepr_mux::persist::PendingSave::channel();
        let first = std::mem::replace(
            &mut app
                .session_saver
                .in_flight
                .as_mut()
                .expect("first checkpoint")
                .pending,
            pending,
        );
        app.cancel_host_shutdown_checkpoint();
        app.request_host_shutdown_checkpoint();
        completion.complete(first.wait());
        app.wait_for_session_save();
        assert!(!app.host_shutdown_checkpoint_result_ready());
        app.start_background_session_save();
        assert!(app.session_saver.save_in_flight());
        app.wait_for_session_save();
        assert_eq!(app.take_host_shutdown_checkpoint_result(), Some(true));
        assert_eq!(saved_pane_counts(&app), vec![2]);
    }

    #[tokio::test]
    async fn two_exits_held_by_one_checkpoint_keep_the_pre_exit_layout_through_teardown() {
        let (mut app, first, second) = two_pane_app("overlapping");
        app.state
            .test_split_workspace(0, shepr_core::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        let reason = shepr_platform::ChildExitReason::Interrupted;
        let first_prepared = app.prepare_pane_exit(first, reason, std::time::Instant::now());
        let second_prepared = app.prepare_pane_exit(second, reason, std::time::Instant::now());
        let first_generation = first_prepared.held_generation().expect("first held");
        let second_generation = second_prepared.held_generation().expect("second held");
        app.wait_for_session_save();
        assert!(app.pane_exit_checkpoint_generation_settled(first_generation));
        assert!(app.pane_exit_checkpoint_generation_settled(second_generation));
        assert!(!app.session_saver.exit.is_requested());
        assert!(app.handle_prepared_pane_exit(interrupted_exit(&app, first), first_prepared));
        assert!(app.handle_prepared_pane_exit(interrupted_exit(&app, second), second_prepared));
        // Both exits were applied, so the three panes saved below are the
        // preserved pre-exit layout, not the live one.
        assert_eq!(app.state.workspaces[0].panes().len(), 1);
        app.sync_session_save_schedule();
        app.save_session_before_teardown_async().await;
        assert_eq!(saved_pane_counts(&app), vec![3]);
        app.retire_session_writer();
    }

    fn directory_files(directory: &std::path::Path) -> Vec<Vec<u8>> {
        let mut paths = std::fs::read_dir(directory)
            .expect("read backup directory")
            .map(|entry| entry.expect("backup directory entry").path())
            .collect::<Vec<_>>();
        paths.sort();
        paths
            .iter()
            .map(|path| std::fs::read(path).expect("read backup"))
            .collect()
    }

    /// A restore that drops a saved workspace leaves that workspace only in the
    /// session file, so the first save copies the file to `session-backups` before
    /// replacing it. The copy is made once, not on every save.
    #[tokio::test]
    async fn a_restore_that_drops_a_workspace_backs_up_the_saved_session_before_the_first_save() {
        use crate::test_support::{AppPathsFixture as _, ValidatedServerConfigFixture as _};
        use shepr_mux::persist::snapshot::{
            DirectionSnapshot, LayoutSnapshot, PaneSnapshot, SessionFile, SessionSnapshot,
            WorkspaceSnapshot,
        };

        let scratch = crate::test_support::ScratchDir::new("dropped-workspace-backup");
        let paths = shepr_paths::AppPaths::test_at(&scratch);
        let config = shepr_config::ValidatedServerConfig::test_from_config_with_paths(
            shepr_config::ServerConfig::default(),
            paths.clone(),
        );
        let data_dir = paths.data_dir().to_path_buf();
        let lease =
            shepr_mux::persist::DataDirLease::acquire(&data_dir).expect("test session lease");

        // A working directory that does not exist: each pane's shell launch
        // fails in its chdir.
        let pane = |public_number| PaneSnapshot {
            cwd: scratch.join("missing-cwd"),
            public_number,
            label: None,
            agent_session: None,
        };
        let workspace =
            |id: &str, name: &str, layout: LayoutSnapshot, ids: &[u32]| WorkspaceSnapshot {
                id: id.parse().expect("workspace id"),
                custom_name: Some(name.into()),
                layout,
                panes: ids
                    .iter()
                    .enumerate()
                    .map(|(index, id)| {
                        (
                            *id,
                            pane(shepr_protocol::PanePublicNumber::new(index + 1).expect("number")),
                        )
                    })
                    .collect(),
                next_public_pane_number: shepr_protocol::PanePublicNumber::new(ids.len() + 1)
                    .expect("next number"),
                zoomed: false,
                focused: ids[0],
                root_pane: ids[0],
            };
        // A saved split ratio out of range refuses the whole file at decode,
        // so the workspace-level defect here is two panes sharing one public
        // number, which drops only that workspace.
        let mut colliding = workspace(
            "w2",
            "colliding numbers",
            LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: shepr_core::layout::SplitRatio::EVEN,
                first: Box::new(LayoutSnapshot::Pane(2)),
                second: Box::new(LayoutSnapshot::Pane(3)),
            },
            &[2, 3],
        );
        for pane in colliding.panes.values_mut() {
            pane.public_number = shepr_protocol::PanePublicNumber::new(1).expect("number");
        }
        let snapshot = SessionSnapshot {
            version: shepr_mux::persist::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![
                workspace("w1", "healthy", LayoutSnapshot::Pane(1), &[1]),
                colliding,
            ],
            active: Some(0),
        };
        let original = serde_json::to_vec(&SessionFile {
            snapshot,
            history_digest: None,
        })
        .expect("encode the saved session");
        // The session file name the persist layer reads and writes.
        let session_file = data_dir.join("session.json");
        std::fs::write(&session_file, &original).expect("test precondition");

        let mut app = App::with_paths(
            &config,
            &paths,
            lease,
            super::super::AppPolicy::Production,
            super::super::tests::test_clock(),
        );
        assert_eq!(
            app.state
                .workspaces
                .iter()
                .map(|workspace| workspace.custom_name.clone())
                .collect::<Vec<_>>(),
            vec![Some("healthy".to_owned())],
            "the saved session loaded and only the invalid workspace was dropped"
        );
        let backups = data_dir.join("session-backups");
        // Every client of this boot is told, naming where the original goes.
        assert_eq!(
            app.restore_notice,
            Some(shepr_protocol::SessionRestoreNotice {
                loss: shepr_protocol::SessionRestoreLoss::Workspaces {
                    dropped: std::num::NonZeroUsize::MIN,
                    panes_pruned: false,
                },
                backup_dir: backups.clone().into(),
            })
        );

        assert!(app.save_session_now(), "first save");
        assert_eq!(directory_files(&backups), vec![original.clone()]);
        let saved = shepr_mux::persist::snapshot::parse_session_file(
            &std::fs::read_to_string(&session_file).expect("read the new session"),
        )
        .expect("parse the new session")
        .snapshot;
        assert_eq!(
            saved
                .workspaces
                .iter()
                .map(|workspace| workspace.custom_name.clone())
                .collect::<Vec<_>>(),
            vec![Some("healthy".to_owned())]
        );

        assert!(app.save_session_now(), "second save");
        assert_eq!(
            directory_files(&backups),
            vec![original],
            "a later save makes no second backup"
        );
    }

    /// A session file that does not parse restores nothing, like a missing
    /// one, but unlike a missing one it is a whole saved session: clients are
    /// told, and the first save backs the file up before replacing it.
    #[test]
    fn an_unusable_session_file_is_reported_and_backed_up() {
        use crate::test_support::{AppPathsFixture as _, ValidatedServerConfigFixture as _};

        let scratch = crate::test_support::ScratchDir::new("unusable-session-notice");
        let paths = shepr_paths::AppPaths::test_at(&scratch);
        let config = shepr_config::ValidatedServerConfig::test_from_config_with_paths(
            shepr_config::ServerConfig::default(),
            paths.clone(),
        );
        let data_dir = paths.data_dir().to_path_buf();
        let lease =
            shepr_mux::persist::DataDirLease::acquire(&data_dir).expect("test session lease");
        let original = b"{ this is not a session".to_vec();
        std::fs::write(data_dir.join("session.json"), &original).expect("test precondition");

        let mut app = App::with_paths(
            &config,
            &paths,
            lease,
            super::super::AppPolicy::Production,
            super::super::tests::test_clock(),
        );
        let backups = data_dir.join("session-backups");
        let Some(shepr_protocol::SessionRestoreNotice {
            loss:
                shepr_protocol::SessionRestoreLoss::Unusable {
                    failure:
                        shepr_protocol::SessionRestoreFailure::Unparseable { line, category, .. },
                },
            backup_dir,
        }) = app.restore_notice.clone()
        else {
            panic!(
                "an unusable session file is reported: {:?}",
                app.restore_notice
            );
        };
        assert_eq!(line, 1);
        assert_eq!(category, shepr_protocol::SessionParseCategory::Syntax);
        assert_eq!(backup_dir.as_path(), backups);

        assert!(app.save_session_now(), "first save");
        assert_eq!(directory_files(&backups), vec![original]);
    }

    #[test]
    fn a_fresh_start_has_nothing_to_report() {
        use crate::test_support::{AppPathsFixture as _, ValidatedServerConfigFixture as _};

        let scratch = crate::test_support::ScratchDir::new("fresh-start-no-notice");
        let paths = shepr_paths::AppPaths::test_at(&scratch);
        let config = shepr_config::ValidatedServerConfig::test_from_config_with_paths(
            shepr_config::ServerConfig::default(),
            paths.clone(),
        );
        let lease = shepr_mux::persist::DataDirLease::acquire(paths.data_dir())
            .expect("test session lease");
        let app = App::with_paths(
            &config,
            &paths,
            lease,
            super::super::AppPolicy::Production,
            super::super::tests::test_clock(),
        );
        assert_eq!(app.restore_notice, None);
    }
}
