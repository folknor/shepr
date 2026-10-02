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

use super::App;
use crate::limits::{CHECKPOINT_MAX_FAILURES, CHECKPOINT_RETRY_MAX_DELAY, SESSION_SAVE_RETRY_MIN};

mod autosave;
mod exit_checkpoint;
mod host_checkpoint;
use autosave::Autosave;
use exit_checkpoint::{PaneExitCheckpoint, PreservedLayout};
use host_checkpoint::HostShutdownCheckpoint;

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
    generation: u64,
    /// The captured layout; `None` when it could not be paired with terminal
    /// identities, or a session mutation was observed after the capture.
    layout: Option<Box<PreservedLayout>>,
}

/// The save `start_background_session_save` should start now.
enum NextSave {
    Autosave,
    Checkpoint {
        exit_generation: Option<u64>,
        host: bool,
    },
}

pub(crate) struct SessionSaver {
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

/// Retry delay of a failed pane-exit or host-shutdown checkpoint after
/// `failures_before` earlier failures of the same checkpoint: doubling from
/// `SESSION_SAVE_RETRY_MIN`, capped at `CHECKPOINT_RETRY_MAX_DELAY`. Total for
/// every `u8`, so raising `CHECKPOINT_MAX_FAILURES` cannot make it overflow.
fn checkpoint_retry_delay(failures_before: u8) -> Duration {
    let factor = 1_u32
        .checked_shl(u32::from(failures_before))
        .unwrap_or(u32::MAX);
    SESSION_SAVE_RETRY_MIN
        .saturating_mul(factor)
        .min(CHECKPOINT_RETRY_MAX_DELAY)
}

impl SessionSaver {
    /// `save_finished` is the signal `persister` was built with.
    pub(crate) fn new(
        persister: shepr_mux::persist::SessionPersister,
        save_finished: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
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
    /// held (the lifecycle freezes saves once it takes that result).
    fn blocked(&self) -> bool {
        self.in_flight.is_some() || (self.host.finished_unsaved() && !self.exit.is_requested())
    }

    /// Whether a checkpoint, rather than an autosave, is the next save.
    fn checkpoint_requested(&self) -> bool {
        self.exit.is_requested() || self.host.is_requested()
    }

    /// When the loop should next try to start a save. A requested checkpoint
    /// reports the later of the two machines' retries, and none when neither
    /// carries one: it starts at once, from the call that requested it or
    /// from the reap of the save before it.
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
    /// that predates the mutation is never installed.
    fn note_mutation(&mut self, now: Instant) {
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
            self.exit.expedite_retry();
        }
        requested
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
    pub(crate) fn freeze_session_saves(&mut self) {
        self.autosave.clear();
        self.exit.release_for_freeze();
    }
}

impl App {
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
            return Some(self.capture_session_save_job());
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
            if self.policy.persists_session() {
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

    /// The one place a save's outcome is applied to the autosave backoff and
    /// both checkpoint machines.
    fn finish_session_save(&mut self, kind: SaveKind, result: std::io::Result<()>) {
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
            Err(error) => {
                // A failed save of any kind re-arms the normal retry; a
                // checkpoint's own retry is its machine's, below.
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
    fn capture_session_save_job(&self) -> shepr_mux::persist::PersistJob {
        if self.state.workspaces.is_empty() {
            shepr_mux::persist::PersistJob::Clear
        } else {
            let (snapshot, cwds) = shepr_mux::persist::capture_deferred(
                &self.state.workspaces,
                &self.state.terminals,
                &self.terminal_runtimes,
                self.paths
                    .current_dir()
                    .unwrap_or_else(|| std::path::Path::new("/")),
                self.state.bookmark_index(),
                self.state.host_terminal_theme,
            );
            let history = self.persist_pane_history.then(|| {
                shepr_mux::persist::capture_pending_history(
                    &self.state.workspaces,
                    &self.terminal_runtimes,
                )
            });
            shepr_mux::persist::PersistJob::Save(shepr_mux::persist::SessionBundle {
                snapshot,
                cwds,
                history,
            })
        }
    }

    /// The layout a pane-exit checkpoint preserves from its capture, or
    /// `None` unless `job` is a save and every pane of every workspace has a
    /// terminal identity to refresh its history and cwd from at the final
    /// save.
    fn capture_preserved_layout(
        &self,
        job: &shepr_mux::persist::PersistJob,
    ) -> Option<PreservedLayout> {
        let shepr_mux::persist::PersistJob::Save(bundle) = job else {
            return None;
        };
        let mut terminal_ids = HashMap::new();
        for (workspace_index, workspace) in self.state.workspaces.iter().enumerate() {
            for pane_id in workspace.panes().keys() {
                let terminal_id = workspace.terminal_id(*pane_id)?.clone();
                terminal_ids.insert((workspace_index, pane_id.raw()), terminal_id);
            }
        }
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
        if !self.policy.persists_session() {
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
                let job = self.capture_session_save_job();
                let ticket = CheckpointTicket {
                    exit: exit_generation.map(|generation| ExitTicket {
                        generation,
                        layout: self.capture_preserved_layout(&job).map(Box::new),
                    }),
                    host,
                };
                self.spawn_session_save(job, SaveKind::Checkpoint(ticket));
            }
            Some(NextSave::Autosave) => {
                self.session_saver.autosave.clear();
                self.spawn_session_save(self.capture_session_save_job(), SaveKind::Autosave);
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

    /// Whether an exited pane may be removed now, without a checkpoint of its
    /// own: nothing is persisted, the latest durable save is a pane-exit
    /// checkpoint and nothing has changed since (so the pane is in it, and a
    /// burst of exits keeps the layout from before the first one), or
    /// checkpoints have been abandoned after repeated failures.
    pub(crate) fn pane_exit_checkpoint_settled(&self) -> bool {
        !self.policy.persists_session()
            || !self.session_saver.exit.would_hold(self.state.session_dirty)
    }

    /// Starts a new checkpoint for a pane exit and returns its generation, or
    /// `None` when the exit is already settled. Each held exit gets a
    /// generation so a save captured before that exit cannot release it when
    /// the save later finishes, and a later change to the session cannot hold
    /// it again.
    pub(crate) fn request_pane_exit_checkpoint(&mut self) -> Option<u64> {
        if !self.policy.persists_session() {
            return None;
        }
        let generation = self.session_saver.exit.request(self.state.session_dirty)?;
        self.start_background_session_save();
        Some(generation)
    }

    /// Whether a held exit's generation is released: a checkpoint holding its
    /// pane is durable, or checkpoints were abandoned or frozen.
    pub(crate) fn pane_exit_checkpoint_generation_settled(&self, generation: u64) -> bool {
        !self.policy.persists_session() || self.session_saver.exit.is_released(generation)
    }

    pub(crate) fn request_host_shutdown_checkpoint(&mut self) {
        if !self.policy.persists_session() || !self.session_saver.request_host_checkpoint() {
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
                self.state.session_dirty = false;
            }
            self.session_saver.autosave.schedule(self.clock.now);
        }
    }

    pub(crate) async fn save_session_before_teardown_async(&mut self) {
        if let Some(save) = self.session_saver.in_flight.take() {
            let result = wait_off_the_runtime(save.pending).await;
            self.finish_session_save(save.kind, result);
        }

        if !self.policy.persists_session() {
            self.session_saver.autosave.clear();
            return;
        }

        let Some(job) = self.capture_final_session_save_job() else {
            self.session_saver.autosave.clear();
            return;
        };
        let pending = self
            .session_saver
            .persister
            .submit(job, self.clock.wall_now);
        let result = wait_off_the_runtime(pending).await;
        let saved = result.is_ok();
        self.finish_session_save(SaveKind::Autosave, result);
        if saved {
            self.session_saver.autosave.clear();
        }
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
async fn wait_off_the_runtime(pending: shepr_mux::persist::PendingSave) -> std::io::Result<()> {
    match tokio::task::spawn_blocking(move || pending.wait()).await {
        Ok(result) => result,
        Err(error) => Err(std::io::Error::other(format!(
            "failed to wait for the session save: {error}"
        ))),
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
        generation: u64,
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

    /// Clears both checkpoint machines' retry deadlines, so the next start
    /// does not wait for them.
    fn expedite_checkpoint_retries(&mut self) {
        self.exit.expedite_retry();
        self.host.expedite_retry();
    }

    /// Private: `PreservedLayout` is only visible inside `session`.
    fn preserved_layout_mut(&mut self) -> Option<&mut PreservedLayout> {
        self.exit.preserved_mut()
    }
}

#[cfg(test)]
impl App {
    /// Blocks until the save in flight, if any, has finished, and records
    /// its outcome.
    pub(super) fn wait_for_session_save(&mut self) {
        if let Some(save) = self.session_saver.in_flight.take() {
            let result = save.pending.wait();
            self.finish_session_save(save.kind, result);
        }
    }

    pub(crate) fn save_session_now(&mut self) -> bool {
        self.wait_for_session_save();

        if !self.policy.persists_session() {
            self.session_saver.autosave.clear();
            return true;
        }

        let job = self.capture_session_save_job();
        let result = self
            .session_saver
            .persister
            .submit(job, self.clock.wall_now)
            .wait();
        let saved = result.is_ok();
        self.finish_session_save(SaveKind::Autosave, result);
        if saved {
            self.session_saver.autosave.clear();
        }
        saved
    }

    /// Delivers `ev` the way the headless loop does: a pane exit that needs a
    /// checkpoint waits for the background save before the app removes it.
    pub(crate) fn handle_internal_event_after_checkpoint(&mut self, ev: AppEvent) {
        let pane_exit_prepared = if let AppEvent::PaneDied {
            pane_id,
            exit_reason,
        } = &ev
        {
            if let Some(generation) = self.prepare_pane_exit(*pane_id, *exit_reason) {
                for _ in 0..4 {
                    self.wait_for_session_save();
                    if self.pane_exit_checkpoint_generation_settled(generation) {
                        break;
                    }
                    self.session_saver.expedite_checkpoint_retries();
                    self.start_background_session_save();
                }
            }
            true
        } else {
            false
        };
        if pane_exit_prepared {
            self.handle_prepared_pane_exit(ev);
        } else {
            self.handle_internal_event(ev);
        }
    }

    /// Save the live pane histories while runtimes still exist, keeping the
    /// directory claim until their processes have finished tearing down.
    pub(crate) fn save_session_before_teardown(&mut self) {
        self.wait_for_session_save();
        if !self.policy.persists_session() {
            self.session_saver.autosave.clear();
            return;
        }
        let Some(job) = self.capture_final_session_save_job() else {
            self.session_saver.autosave.clear();
            return;
        };
        let result = self
            .session_saver
            .persister
            .submit(job, self.clock.wall_now)
            .wait();
        let saved = result.is_ok();
        self.finish_session_save(SaveKind::Autosave, result);
        if saved {
            self.session_saver.autosave.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(
            &shepr_config::ServerConfig::default(),
            super::super::AppPolicy::Test,
        )
    }

    /// A production-policy app with one workspace of two panes, returning the
    /// pane that will exit.
    fn two_pane_app(name: &str) -> (App, shepr_core::layout::PaneId, shepr_core::layout::PaneId) {
        use crate::test_support::WorkspaceFixture as _;
        let mut app = test_app();
        app.policy = super::super::AppPolicy::Production;
        let mut workspace = shepr_mux::workspace::Workspace::test_new(name);
        let exiting = workspace.root_pane();
        let staying = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        app.state.workspaces = vec![workspace];
        app.state.set_bookmark_index(Some(0));
        app.state.ensure_test_terminals();
        (app, exiting, staying)
    }

    fn saved_pane_counts(app: &App) -> Vec<usize> {
        let saved = std::fs::read_to_string(
            app.paths
                .data_dir()
                .join(shepr_mux::persist::SessionWriter::SESSION_FILE_NAME),
        )
        .expect("read the session file");
        shepr_mux::persist::snapshot::parse_snapshot(&saved)
            .expect("parse the session file")
            .workspaces
            .iter()
            .map(|workspace| workspace.panes.len())
            .collect()
    }

    #[test]
    fn a_held_pane_exit_settles_on_its_checkpoint_although_the_session_changed_meanwhile() {
        let (mut app, exiting, _) = two_pane_app("held");

        let generation = app
            .prepare_pane_exit(exiting, shepr_platform::ChildExitReason::Interrupted)
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
        app.policy = super::super::AppPolicy::Test;
    }

    #[test]
    fn a_save_captured_before_a_pane_exit_does_not_settle_it() {
        let (mut app, exiting, _) = two_pane_app("earlier-save");
        let earlier = app.session_saver.hold_test_save_in_flight();

        let generation = app
            .prepare_pane_exit(exiting, shepr_platform::ChildExitReason::Interrupted)
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
        app.policy = super::super::AppPolicy::Test;
    }

    #[test]
    fn exits_after_a_pane_exit_checkpoint_keep_its_layout() {
        let (mut app, exiting, staying) = two_pane_app("burst");

        app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
            pane_id: exiting,
            exit_reason: shepr_platform::ChildExitReason::Interrupted,
        });
        assert_eq!(saved_pane_counts(&app), vec![2]);
        assert_eq!(
            app.prepare_pane_exit(staying, shepr_platform::ChildExitReason::Interrupted),
            None,
            "the checkpoint on disk already holds the second pane"
        );
        app.policy = super::super::AppPolicy::Test;
    }

    #[tokio::test]
    async fn the_final_save_rewrites_the_checkpoint_layout_instead_of_skipping_it() {
        let (mut app, exiting, _) = two_pane_app("final");
        app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
            pane_id: exiting,
            exit_reason: shepr_platform::ChildExitReason::Interrupted,
        });
        assert_eq!(app.state.workspaces[0].panes().len(), 1);
        // A final save that skipped would leave no session file behind.
        std::fs::remove_file(
            app.paths
                .data_dir()
                .join(shepr_mux::persist::SessionWriter::SESSION_FILE_NAME),
        )
        .expect("remove the checkpoint");

        app.save_session_before_teardown_async().await;

        assert_eq!(
            saved_pane_counts(&app),
            vec![2],
            "the final save keeps the layout the checkpoint saved"
        );
        app.retire_session_writer();
        app.policy = super::super::AppPolicy::Test;
    }

    #[tokio::test]
    async fn a_final_save_with_missing_checkpoint_identities_keeps_the_durable_layout() {
        let (mut app, exiting, _) = two_pane_app("identities");
        app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
            pane_id: exiting,
            exit_reason: shepr_platform::ChildExitReason::Interrupted,
        });
        app.session_saver
            .preserved_layout_mut()
            .expect("saved checkpoint")
            .terminal_ids
            .clear();

        app.save_session_before_teardown_async().await;

        assert_eq!(saved_pane_counts(&app), vec![2]);
        assert!(app.preserves_pane_exit_checkpoint());
        app.retire_session_writer();
        app.policy = super::super::AppPolicy::Test;
    }

    fn exit_kind(generation: u64) -> SaveKind {
        SaveKind::Checkpoint(CheckpointTicket {
            exit: Some(ExitTicket {
                generation,
                layout: None,
            }),
            host: false,
        })
    }

    fn disk_full() -> std::io::Result<()> {
        Err(std::io::Error::other("disk full"))
    }

    #[test]
    fn repeated_pane_exit_checkpoint_failures_release_the_held_exit() {
        let mut app = test_app();
        app.policy = super::super::AppPolicy::Production;
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
        app.policy = super::super::AppPolicy::Test;
    }

    #[test]
    fn a_requested_checkpoint_is_chosen_over_a_due_autosave() {
        let mut app = test_app();
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
        let mut app = test_app();
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
        let mut app = test_app();
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
        let mut app = test_app();
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
        app.session_saver.freeze_session_saves();
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
        app.policy = super::super::AppPolicy::Test;
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
        app.policy = super::super::AppPolicy::Test;
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
        app.policy = super::super::AppPolicy::Test;
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
        app.policy = super::super::AppPolicy::Test;
    }

    #[test]
    fn a_host_checkpoint_supersedes_a_preserved_pane_exit_layout() {
        let (mut app, exiting, _) = two_pane_app("host-supersedes");
        app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
            pane_id: exiting,
            exit_reason: shepr_platform::ChildExitReason::Interrupted,
        });
        assert!(app.preserves_pane_exit_checkpoint());
        app.request_host_shutdown_checkpoint();
        app.wait_for_session_save();
        assert_eq!(saved_pane_counts(&app), vec![1]);
        assert!(!app.preserves_pane_exit_checkpoint());
        assert_eq!(app.take_host_shutdown_checkpoint_result(), Some(true));
        app.policy = super::super::AppPolicy::Test;
    }

    #[test]
    fn a_mutation_pending_when_a_checkpoint_starts_discards_the_preserved_layout() {
        let (mut app, exiting, _) = two_pane_app("pending-mutation");
        app.handle_internal_event_after_checkpoint(AppEvent::PaneDied {
            pane_id: exiting,
            exit_reason: shepr_platform::ChildExitReason::Interrupted,
        });
        assert!(app.preserves_pane_exit_checkpoint());
        app.state.mark_session_dirty();
        app.request_host_shutdown_checkpoint();
        assert!(app.session_saver.save_in_flight());
        assert!(!app.preserves_pane_exit_checkpoint());
        assert!(app.session_saver.exit.preserved().is_none());
        app.wait_for_session_save();
        app.policy = super::super::AppPolicy::Test;
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
        app.policy = super::super::AppPolicy::Test;
    }

    #[tokio::test]
    async fn two_exits_held_by_one_checkpoint_keep_the_pre_exit_layout_through_teardown() {
        use crate::test_support::WorkspaceFixture as _;
        let (mut app, first, second) = two_pane_app("overlapping");
        app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        let reason = shepr_platform::ChildExitReason::Interrupted;
        let first_generation = app.prepare_pane_exit(first, reason).expect("first held");
        let second_generation = app.prepare_pane_exit(second, reason).expect("second held");
        app.wait_for_session_save();
        assert!(app.pane_exit_checkpoint_generation_settled(first_generation));
        assert!(app.pane_exit_checkpoint_generation_settled(second_generation));
        assert!(!app.session_saver.exit.is_requested());
        app.handle_prepared_pane_exit(AppEvent::PaneDied {
            pane_id: first,
            exit_reason: reason,
        });
        app.handle_prepared_pane_exit(AppEvent::PaneDied {
            pane_id: second,
            exit_reason: reason,
        });
        app.sync_session_save_schedule();
        app.save_session_before_teardown_async().await;
        assert_eq!(saved_pane_counts(&app), vec![3]);
        app.retire_session_writer();
        app.policy = super::super::AppPolicy::Test;
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
            DirectionSnapshot, LayoutSnapshot, PaneSnapshot, SessionSnapshot, WorkspaceSnapshot,
        };

        let scratch = crate::test_support::ScratchDir::new("dropped-workspace-backup");
        let paths = shepr_config::AppPaths::test_at(&scratch);
        let config = shepr_config::ValidatedServerConfig::test_from_config_with_paths(
            shepr_config::ServerConfig::default(),
            paths.clone(),
        );
        let data_dir = paths.data_dir().to_path_buf();
        let lease =
            shepr_mux::persist::DataDirLease::acquire(&data_dir).expect("test session lease");

        // A working directory that does not exist: each pane's shell launch
        // fails in its chdir.
        let pane = || PaneSnapshot {
            cwd: scratch.join("missing-cwd"),
            public_number: None,
            label: None,
            agent_session: None,
        };
        let workspace =
            |id: &str, name: &str, layout: LayoutSnapshot, ids: &[u32]| WorkspaceSnapshot {
                id: Some(id.into()),
                custom_name: Some(name.into()),
                identity_cwd: scratch.path().to_path_buf(),
                next_public_pane_number: 0,
                layout,
                panes: ids.iter().map(|id| (*id, pane())).collect(),
                zoomed: false,
                focused: None,
                root_pane: None,
            };
        let snapshot = SessionSnapshot {
            version: shepr_mux::persist::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![
                workspace("w1", "healthy", LayoutSnapshot::Pane(1), &[1]),
                workspace(
                    "w2",
                    "invalid ratio",
                    LayoutSnapshot::Split {
                        direction: DirectionSnapshot::Horizontal,
                        ratio: 1.0,
                        first: Box::new(LayoutSnapshot::Pane(2)),
                        second: Box::new(LayoutSnapshot::Pane(3)),
                    },
                    &[2, 3],
                ),
            ],
            active: Some(0),
        };
        let original = serde_json::to_vec(&snapshot).expect("encode the saved session");
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
                unusable: None,
                dropped_workspaces: 1,
                panes_pruned: false,
                backup_dir: backups.display().to_string(),
            })
        );

        assert!(app.save_session_now(), "first save");
        assert_eq!(directory_files(&backups), vec![original.clone()]);
        let saved = shepr_mux::persist::snapshot::parse_snapshot(
            &std::fs::read_to_string(&session_file).expect("read the new session"),
        )
        .expect("parse the new session");
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
        app.policy = super::super::AppPolicy::Test;
    }

    /// A session file that does not parse restores nothing, like a missing
    /// one, but unlike a missing one it is a whole saved session: clients are
    /// told, and the first save backs the file up before replacing it.
    #[test]
    fn an_unusable_session_file_is_reported_and_backed_up() {
        use crate::test_support::{AppPathsFixture as _, ValidatedServerConfigFixture as _};

        let scratch = crate::test_support::ScratchDir::new("unusable-session-notice");
        let paths = shepr_config::AppPaths::test_at(&scratch);
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
            unusable: Some(reason),
            dropped_workspaces: 0,
            panes_pruned: false,
            backup_dir,
        }) = app.restore_notice.clone()
        else {
            panic!(
                "an unusable session file is reported: {:?}",
                app.restore_notice
            );
        };
        assert!(reason.contains("parsed"), "{reason}");
        assert_eq!(backup_dir, backups.display().to_string());

        assert!(app.save_session_now(), "first save");
        assert_eq!(directory_files(&backups), vec![original]);
        app.policy = super::super::AppPolicy::Test;
    }

    #[test]
    fn a_fresh_start_has_nothing_to_report() {
        use crate::test_support::{AppPathsFixture as _, ValidatedServerConfigFixture as _};

        let scratch = crate::test_support::ScratchDir::new("fresh-start-no-notice");
        let paths = shepr_config::AppPaths::test_at(&scratch);
        let config = shepr_config::ValidatedServerConfig::test_from_config_with_paths(
            shepr_config::ServerConfig::default(),
            paths.clone(),
        );
        let lease = shepr_mux::persist::DataDirLease::acquire(paths.data_dir())
            .expect("test session lease");
        let mut app = App::with_paths(
            &config,
            &paths,
            lease,
            super::super::AppPolicy::Production,
            super::super::tests::test_clock(),
        );
        assert_eq!(app.restore_notice, None);
        app.policy = super::super::AppPolicy::Test;
    }
}
