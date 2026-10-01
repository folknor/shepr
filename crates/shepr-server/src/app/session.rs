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
use crate::limits::{
    CHECKPOINT_MAX_FAILURES, HOST_SHUTDOWN_CHECKPOINT_RETRY_MAX_DELAY, SESSION_SAVE_DEBOUNCE,
    SESSION_SAVE_RETRY_MAX, SESSION_SAVE_RETRY_MIN,
};
#[derive(Clone, Copy)]
enum SessionSavePurpose {
    Autosave,
    Checkpoint {
        host_shutdown_generation: Option<u64>,
        pane_exit_generation: Option<u64>,
    },
}

/// The layout and pane identities from a successful pane-exit checkpoint.
/// Shutdown can keep this layout while refreshing live panes' history and
/// cwd, even after exited panes have left the in-memory workspace.
struct PaneExitCheckpointSnapshot {
    generation: u64,
    session_revision: u64,
    snapshot: shepr_mux::persist::SessionSnapshot,
    terminal_ids: HashMap<(usize, u32), shepr_protocol::TerminalId>,
}

/// A save the persister is running, and why it was taken.
struct InFlightSave {
    pending: shepr_mux::persist::PendingSave,
    purpose: SessionSavePurpose,
    pane_exit_snapshot: Option<PaneExitCheckpointSnapshot>,
}

pub(crate) struct SessionSaver {
    pub(crate) session_save_deadline: Option<Instant>,
    /// Consecutive failed saves, for the retry backoff.
    failed_saves: u32,
    /// At most one save is in flight: a due save waits for it, so every
    /// capture reaches the persister after the one before it finished.
    in_flight: Option<InFlightSave>,
    persister: shepr_mux::persist::SessionPersister,
    /// Fired by the persister each time a submitted save ends.
    save_finished: Arc<tokio::sync::Notify>,
    pane_exit_checkpoint_requested: bool,
    pane_exit_checkpoint_generation: u64,
    pane_exit_checkpoint_saved_generation: u64,
    // Presence is the preservation state: a protected layout always owns its
    // snapshot, and invalidation drops both in one operation.
    pane_exit_checkpoint_snapshot: Option<PaneExitCheckpointSnapshot>,
    session_revision: u64,
    /// Consecutive failed pane-exit checkpoints. At the bound, exited panes are
    /// removed without a checkpoint until any save succeeds again.
    pane_exit_checkpoint_failures: u8,
    pub(crate) pane_exit_checkpoint_ready: bool,
    critical_save_retry_deadline: Option<Instant>,
    host_shutdown_checkpoint_generation: u64,
    host_shutdown_checkpoint_requested: bool,
    host_shutdown_checkpoint_failures: u8,
    host_shutdown_checkpoint_result: Option<(u64, bool)>,
}

impl SessionSaver {
    /// `save_finished` is the signal `persister` was built with.
    pub(crate) fn new(
        persister: shepr_mux::persist::SessionPersister,
        save_finished: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            session_save_deadline: None,
            failed_saves: 0,
            in_flight: None,
            persister,
            save_finished,
            pane_exit_checkpoint_requested: false,
            pane_exit_checkpoint_generation: 0,
            pane_exit_checkpoint_saved_generation: 0,
            pane_exit_checkpoint_snapshot: None,
            session_revision: 0,
            pane_exit_checkpoint_failures: 0,
            pane_exit_checkpoint_ready: false,
            critical_save_retry_deadline: None,
            host_shutdown_checkpoint_generation: 0,
            host_shutdown_checkpoint_requested: false,
            host_shutdown_checkpoint_failures: 0,
            host_shutdown_checkpoint_result: None,
        }
    }

    /// When the loop should next try to start a save. `None` while a save is
    /// in flight: nothing can start before it ends, and its end fires
    /// [`Self::save_finished`], which wakes the loop instead.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        if self.in_flight.is_some() {
            return None;
        }
        [
            self.session_save_deadline,
            self.critical_save_retry_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(crate) fn is_due(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| now >= deadline)
    }

    /// The signal the persister fires when a save ends. The headless loop
    /// waits on it; a firing with nothing to reap is harmless.
    pub(crate) fn save_finished(&self) -> &tokio::sync::Notify {
        &self.save_finished
    }

    pub(crate) fn clear_deadline(&mut self) {
        self.session_save_deadline = None;
        self.critical_save_retry_deadline = None;
    }

    /// Stops scheduled saves for a host shutdown. Exits held for a pane-exit
    /// checkpoint are released: the host-shutdown checkpoint already captured
    /// the layout they were held in (or ran out of retries), and nothing may
    /// write the session once it is frozen.
    pub(crate) fn freeze_session_saves(&mut self) {
        self.clear_deadline();
        if self.pane_exit_checkpoint_requested {
            self.pane_exit_checkpoint_requested = false;
            self.pane_exit_checkpoint_ready = true;
        }
    }

    fn schedule(&mut self, now: Instant) {
        self.session_revision = self.session_revision.saturating_add(1);
        self.pane_exit_checkpoint_snapshot = None;
        self.session_save_deadline = Some(now + SESSION_SAVE_DEBOUNCE);
    }

    /// Schedules the retry for a failed save. The delay doubles per
    /// consecutive failure up to a cap, so a persistent failure (a full disk,
    /// a directory where the history file belongs) does not re-capture and
    /// rewrite the whole session four times a second.
    fn retry_after_failure(&mut self, now: Instant) -> Duration {
        self.failed_saves = self.failed_saves.saturating_add(1);
        let exponent = self.failed_saves.saturating_sub(1).min(16);
        let delay = SESSION_SAVE_RETRY_MIN
            .saturating_mul(1 << exponent)
            .min(SESSION_SAVE_RETRY_MAX);
        self.retry_after(now, delay);
        delay
    }

    fn retry_after(&mut self, now: Instant, delay: Duration) {
        let retry = now + delay;
        self.session_save_deadline = Some(
            self.session_save_deadline
                .filter(|deadline| *deadline > now)
                .map_or(retry, |deadline| deadline.min(retry)),
        );
    }

    fn save_is_due(&self, now: Instant) -> bool {
        self.session_save_deadline
            .is_some_and(|deadline| now >= deadline)
    }

    fn critical_save_is_due(&self, now: Instant) -> bool {
        self.critical_save_retry_deadline
            .is_none_or(|deadline| now >= deadline)
    }
}

impl App {
    pub(super) fn preserves_pane_exit_checkpoint(&self) -> bool {
        self.session_saver.pane_exit_checkpoint_snapshot.is_some() && !self.state.session_dirty
    }

    // A missing identity must never replace the protected layout with the
    // post-exit layout. Keep the durable checkpoint if refreshing it fails.
    fn capture_final_session_save_job(&self) -> Option<shepr_mux::persist::PersistJob> {
        if !self.preserves_pane_exit_checkpoint() {
            return Some(self.capture_session_save_job());
        }
        let checkpoint = self.session_saver.pane_exit_checkpoint_snapshot.as_ref()?;
        if checkpoint.generation != self.session_saver.pane_exit_checkpoint_saved_generation {
            tracing::warn!(
                "pane-exit checkpoint generation mismatch; keeping the durable checkpoint"
            );
            return None;
        }
        let job = self.capture_save_job_from_pane_exit_checkpoint(checkpoint);
        if job.is_none() {
            tracing::warn!(
                "could not pair fresh pane history with the saved pane-exit layout; keeping the durable checkpoint"
            );
        }
        job
    }

    fn finish_final_session_save(&mut self, result: std::io::Result<()>) {
        if self.record_session_save_result(result, self.clock.now) {
            self.session_saver.pane_exit_checkpoint_snapshot = None;
            self.session_saver.clear_deadline();
        }
    }

    /// Consumes the pure state mutation signal and applies persistence effects
    /// once for this loop pass. AppState mutations and App-owned mutations use
    /// the same flag, so a handler cannot schedule the same change twice.
    pub(crate) fn sync_session_save_schedule(&mut self) {
        if self.state.session_dirty {
            self.state.session_dirty = false;
            if self.policy.persists_session() {
                self.session_saver.schedule(self.clock.now);
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
            self.finish_session_save_with_checkpoint(
                save.purpose,
                result,
                self.clock.now,
                save.pane_exit_snapshot,
            );
        }
        true
    }

    pub(super) fn record_session_save_result(
        &mut self,
        result: std::io::Result<()>,
        now: Instant,
    ) -> bool {
        self.record_session_save_result_with_retry(result, now).0
    }

    fn record_session_save_result_with_retry(
        &mut self,
        result: std::io::Result<()>,
        now: Instant,
    ) -> (bool, Option<Duration>) {
        self.record_session_save_result_with_retry_delay(result, now, None)
    }

    fn record_session_save_result_with_retry_delay(
        &mut self,
        result: std::io::Result<()>,
        now: Instant,
        retry_delay_override: Option<Duration>,
    ) -> (bool, Option<Duration>) {
        match result {
            Err(err) => {
                let backoff_delay = self.session_saver.retry_after_failure(now);
                let delay = retry_delay_override.unwrap_or(backoff_delay);
                tracing::warn!(
                    error = %err,
                    failures = self.session_saver.failed_saves,
                    retry_ms = delay.as_millis(),
                    "session save failed"
                );
                (false, Some(delay))
            }
            Ok(()) => {
                self.session_saver.pane_exit_checkpoint_failures = 0;
                if self.session_saver.failed_saves > 0 {
                    tracing::info!(
                        failures = self.session_saver.failed_saves,
                        "session save recovered after failures"
                    );
                    self.session_saver.failed_saves = 0;
                }
                (true, None)
            }
        }
    }

    fn finish_session_save_with_checkpoint(
        &mut self,
        purpose: SessionSavePurpose,
        result: std::io::Result<()>,
        now: Instant,
        pane_exit_snapshot: Option<PaneExitCheckpointSnapshot>,
    ) {
        let retry_delay_override = match purpose {
            SessionSavePurpose::Checkpoint {
                host_shutdown_generation: Some(_),
                ..
            } => {
                let exponent =
                    u32::from(self.session_saver.host_shutdown_checkpoint_failures.min(2));
                Some(
                    SESSION_SAVE_RETRY_MIN
                        .saturating_mul(1_u32 << exponent)
                        .min(HOST_SHUTDOWN_CHECKPOINT_RETRY_MAX_DELAY),
                )
            }
            SessionSavePurpose::Autosave
            | SessionSavePurpose::Checkpoint {
                host_shutdown_generation: None,
                ..
            } => None,
        };
        let (saved, retry_delay) =
            self.record_session_save_result_with_retry_delay(result, now, retry_delay_override);
        match purpose {
            SessionSavePurpose::Autosave => {
                if saved
                    && !self.session_saver.pane_exit_checkpoint_requested
                    && !self.session_saver.host_shutdown_checkpoint_requested
                {
                    self.session_saver.pane_exit_checkpoint_snapshot = None;
                }
                if self.session_saver.host_shutdown_checkpoint_requested {
                    self.session_saver.critical_save_retry_deadline = None;
                } else if self.session_saver.pane_exit_checkpoint_requested {
                    if saved {
                        self.session_saver.critical_save_retry_deadline = None;
                    } else if let Some(delay) = retry_delay {
                        self.session_saver.critical_save_retry_deadline = Some(now + delay);
                    }
                }
            }
            SessionSavePurpose::Checkpoint {
                host_shutdown_generation,
                pane_exit_generation,
            } => {
                if saved {
                    self.session_saver.critical_save_retry_deadline = None;
                    if let Some(generation) = pane_exit_generation {
                        self.record_pane_exit_checkpoint_saved(generation, pane_exit_snapshot);
                        if generation == self.session_saver.pane_exit_checkpoint_generation
                            && self.session_saver.pane_exit_checkpoint_requested
                        {
                            self.session_saver.pane_exit_checkpoint_requested = false;
                            self.session_saver.pane_exit_checkpoint_failures = 0;
                        }
                    }
                    if self.session_saver.pane_exit_checkpoint_requested {
                        self.session_saver.session_save_deadline = None;
                    }
                    if host_shutdown_generation.is_some()
                        && pane_exit_generation.is_none()
                        && !self.session_saver.pane_exit_checkpoint_requested
                    {
                        self.session_saver.pane_exit_checkpoint_snapshot = None;
                    }
                    if let Some(generation) = host_shutdown_generation
                        && generation == self.session_saver.host_shutdown_checkpoint_generation
                        && self.session_saver.host_shutdown_checkpoint_requested
                    {
                        self.session_saver.host_shutdown_checkpoint_requested = false;
                        self.session_saver.host_shutdown_checkpoint_failures = 0;
                        self.session_saver.host_shutdown_checkpoint_result =
                            Some((generation, true));
                    }
                } else if let Some(delay) = retry_delay {
                    let pane_exit_abandoned = pane_exit_generation
                        == Some(self.session_saver.pane_exit_checkpoint_generation)
                        && self.session_saver.pane_exit_checkpoint_requested
                        && self.record_failed_pane_exit_checkpoint();
                    if let Some(generation) = host_shutdown_generation
                        && generation == self.session_saver.host_shutdown_checkpoint_generation
                        && self.session_saver.host_shutdown_checkpoint_requested
                    {
                        self.session_saver.host_shutdown_checkpoint_failures = self
                            .session_saver
                            .host_shutdown_checkpoint_failures
                            .saturating_add(1);
                        if self.session_saver.host_shutdown_checkpoint_failures
                            >= CHECKPOINT_MAX_FAILURES
                        {
                            self.session_saver.host_shutdown_checkpoint_requested = false;
                            self.session_saver.host_shutdown_checkpoint_result =
                                Some((generation, false));
                            self.session_saver.critical_save_retry_deadline = None;
                        } else {
                            self.session_saver.critical_save_retry_deadline = Some(now + delay);
                        }
                    } else if self.session_saver.host_shutdown_checkpoint_requested {
                        self.session_saver.critical_save_retry_deadline = None;
                    } else if self.session_saver.pane_exit_checkpoint_requested {
                        self.session_saver.critical_save_retry_deadline = Some(now + delay);
                    } else if pane_exit_abandoned {
                        self.session_saver.critical_save_retry_deadline = None;
                    }
                }
            }
        }
    }

    /// Counts a failed pane-exit checkpoint. At the bound it stops holding the
    /// exited panes: they are released for removal without a checkpoint, and
    /// later exits skip it too until some save succeeds.
    fn record_failed_pane_exit_checkpoint(&mut self) -> bool {
        let saver = &mut self.session_saver;
        saver.pane_exit_checkpoint_failures = saver.pane_exit_checkpoint_failures.saturating_add(1);
        if saver.pane_exit_checkpoint_failures < CHECKPOINT_MAX_FAILURES {
            return false;
        }
        tracing::warn!(
            failures = saver.pane_exit_checkpoint_failures,
            "pane exit checkpoint failed repeatedly; removing exited panes without persisting their exit"
        );
        saver.pane_exit_checkpoint_requested = false;
        saver.pane_exit_checkpoint_ready = true;
        true
    }

    fn record_pane_exit_checkpoint_saved(
        &mut self,
        generation: u64,
        snapshot: Option<PaneExitCheckpointSnapshot>,
    ) {
        self.session_saver.pane_exit_checkpoint_saved_generation = self
            .session_saver
            .pane_exit_checkpoint_saved_generation
            .max(generation);
        if let Some(snapshot) = snapshot {
            let current_snapshot = &self.session_saver.pane_exit_checkpoint_snapshot;
            if snapshot.session_revision == self.session_saver.session_revision
                && current_snapshot
                    .as_ref()
                    .is_none_or(|current| snapshot.generation >= current.generation)
            {
                self.session_saver.pane_exit_checkpoint_snapshot = Some(snapshot);
            }
        }
        self.session_saver.pane_exit_checkpoint_ready = true;
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

    fn capture_pane_exit_checkpoint_snapshot(
        &self,
        generation: u64,
        job: &shepr_mux::persist::PersistJob,
    ) -> Option<PaneExitCheckpointSnapshot> {
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
        Some(PaneExitCheckpointSnapshot {
            generation,
            session_revision: self.session_saver.session_revision,
            snapshot: bundle.snapshot.clone(),
            terminal_ids,
        })
    }

    fn capture_save_job_from_pane_exit_checkpoint(
        &self,
        checkpoint: &PaneExitCheckpointSnapshot,
    ) -> Option<shepr_mux::persist::PersistJob> {
        let cwds = shepr_mux::persist::snapshot::capture_pending_cwds_for_snapshot(
            &checkpoint.snapshot,
            &checkpoint.terminal_ids,
            &self.terminal_runtimes,
        )?;
        let history = if self.persist_pane_history {
            Some(
                shepr_mux::persist::snapshot::capture_pending_history_for_snapshot(
                    &checkpoint.snapshot,
                    &checkpoint.terminal_ids,
                    &self.terminal_runtimes,
                )?,
            )
        } else {
            None
        };
        Some(shepr_mux::persist::PersistJob::Save(
            shepr_mux::persist::SessionBundle {
                snapshot: checkpoint.snapshot.clone(),
                cwds,
                history,
            },
        ))
    }

    pub(crate) fn start_background_session_save(&mut self) {
        if !self.policy.persists_session() && !self.session_saver.pane_exit_checkpoint_requested {
            self.session_saver.clear_deadline();
            return;
        }

        let now = self.clock.now;
        self.reap_finished_session_save();
        if self
            .session_saver
            .host_shutdown_checkpoint_result
            .is_some_and(|(_, saved)| !saved)
            && !self.session_saver.pane_exit_checkpoint_requested
        {
            return;
        }
        if self.session_saver.in_flight.is_some() {
            // A due save or checkpoint keeps its deadline: the end of the
            // save in flight wakes the loop, which reaps it and comes back
            // here to start this one.
            return;
        }

        let host_shutdown_generation = self
            .session_saver
            .host_shutdown_checkpoint_requested
            .then_some(self.session_saver.host_shutdown_checkpoint_generation);
        let pane_exit_generation = self
            .session_saver
            .pane_exit_checkpoint_requested
            .then_some(self.session_saver.pane_exit_checkpoint_generation);
        let checkpoint_requested =
            self.session_saver.pane_exit_checkpoint_requested || host_shutdown_generation.is_some();
        if checkpoint_requested {
            if !self.session_saver.critical_save_is_due(now) {
                return;
            }
            self.session_saver.session_save_deadline = None;
            self.session_saver.critical_save_retry_deadline = None;
            self.state.session_dirty = false;
            let job = self.capture_session_save_job();
            let pane_exit_snapshot = pane_exit_generation.and_then(|generation| {
                self.capture_pane_exit_checkpoint_snapshot(generation, &job)
            });
            self.spawn_session_save(
                job,
                SessionSavePurpose::Checkpoint {
                    host_shutdown_generation,
                    pane_exit_generation,
                },
                pane_exit_snapshot,
            );
        } else if self.session_saver.save_is_due(now) {
            self.session_saver.session_save_deadline = None;
            self.spawn_session_save(
                self.capture_session_save_job(),
                SessionSavePurpose::Autosave,
                None,
            );
        }
    }

    fn spawn_session_save(
        &mut self,
        job: shepr_mux::persist::PersistJob,
        purpose: SessionSavePurpose,
        pane_exit_snapshot: Option<PaneExitCheckpointSnapshot>,
    ) {
        let pending = self
            .session_saver
            .persister
            .submit(job, self.clock.wall_now);
        self.session_saver.in_flight = Some(InFlightSave {
            pending,
            purpose,
            pane_exit_snapshot,
        });
    }

    /// Whether an exited pane may be removed now, without a checkpoint of its
    /// own: nothing is persisted, the latest durable save is a pane-exit
    /// checkpoint and nothing has changed since (so the pane is in it, and a
    /// burst of exits keeps the layout from before the first one), or
    /// checkpoints have been abandoned after repeated failures.
    pub(crate) fn pane_exit_checkpoint_settled(&self) -> bool {
        let saver = &self.session_saver;
        (!self.policy.persists_session() && !saver.pane_exit_checkpoint_requested)
            || (saver.pane_exit_checkpoint_snapshot.is_some() && !self.state.session_dirty)
            || saver.pane_exit_checkpoint_failures >= CHECKPOINT_MAX_FAILURES
    }

    /// Starts a new checkpoint for a pane exit and returns its generation, or
    /// `None` when the exit is already settled. Each held exit gets a
    /// generation so a save captured before that exit cannot release it when
    /// the save later finishes, and a later change to the session cannot hold
    /// it again.
    pub(crate) fn request_pane_exit_checkpoint(&mut self) -> Option<u64> {
        if self.pane_exit_checkpoint_settled() {
            return None;
        }
        self.session_saver.pane_exit_checkpoint_requested = true;
        self.session_saver.pane_exit_checkpoint_generation = self
            .session_saver
            .pane_exit_checkpoint_generation
            .saturating_add(1);
        let generation = self.session_saver.pane_exit_checkpoint_generation;
        self.start_background_session_save();
        Some(generation)
    }

    /// Whether a particular held exit has its own durable checkpoint.
    pub(crate) fn pane_exit_checkpoint_generation_settled(&self, generation: u64) -> bool {
        self.session_saver.pane_exit_checkpoint_saved_generation >= generation
            || self.session_saver.pane_exit_checkpoint_failures >= CHECKPOINT_MAX_FAILURES
            || (!self.policy.persists_session()
                && !self.session_saver.pane_exit_checkpoint_requested)
    }

    pub(crate) fn take_pane_exit_checkpoint_ready(&mut self) -> bool {
        std::mem::take(&mut self.session_saver.pane_exit_checkpoint_ready)
    }

    pub(crate) fn pane_exit_checkpoint_requested(&self) -> bool {
        self.session_saver.pane_exit_checkpoint_requested
    }

    pub(crate) fn request_host_shutdown_checkpoint(&mut self) {
        if !self.policy.persists_session()
            || self.session_saver.host_shutdown_checkpoint_requested
            || self.session_saver.host_shutdown_checkpoint_result.is_some()
        {
            return;
        }
        self.session_saver.host_shutdown_checkpoint_generation = self
            .session_saver
            .host_shutdown_checkpoint_generation
            .saturating_add(1);
        self.session_saver.host_shutdown_checkpoint_requested = true;
        self.session_saver.host_shutdown_checkpoint_failures = 0;
        self.session_saver.critical_save_retry_deadline = None;
        self.start_background_session_save();
    }

    pub(crate) fn host_shutdown_checkpoint_result_ready(&self) -> bool {
        self.session_saver.host_shutdown_checkpoint_result.is_some()
    }

    pub(crate) fn take_host_shutdown_checkpoint_result(&mut self) -> Option<bool> {
        self.session_saver
            .host_shutdown_checkpoint_result
            .take()
            .and_then(|(generation, saved)| {
                (generation == self.session_saver.host_shutdown_checkpoint_generation)
                    .then_some(saved)
            })
    }

    pub(crate) fn cancel_host_shutdown_checkpoint(&mut self) {
        self.session_saver.host_shutdown_checkpoint_generation = self
            .session_saver
            .host_shutdown_checkpoint_generation
            .saturating_add(1);
        self.session_saver.host_shutdown_checkpoint_requested = false;
        self.session_saver.host_shutdown_checkpoint_failures = 0;
        self.session_saver.host_shutdown_checkpoint_result = None;
        self.session_saver.critical_save_retry_deadline = None;
    }

    pub(crate) fn finish_checkpointed_pane_exit(&mut self) {
        self.finish_checkpointed_pane_exit_after_event(false);
    }

    pub(crate) fn finish_checkpointed_pane_exit_after_event(&mut self, session_was_dirty: bool) {
        if self.session_saver.pane_exit_checkpoint_snapshot.is_some() {
            if session_was_dirty {
                // Preserve mutations that arrived after the checkpoint capture.
                // They need a current-state save even though the exit itself
                // was safe to replay from its generation's checkpoint.
                self.session_saver.pane_exit_checkpoint_snapshot = None;
            } else {
                // Removing the already-checkpointed pane is not a new durable
                // mutation; the later save can update the layout after debounce.
                self.state.session_dirty = false;
            }
            self.session_saver.session_save_deadline = Some(self.clock.now + SESSION_SAVE_DEBOUNCE);
        }
    }

    pub(crate) async fn save_session_before_teardown_async(&mut self) {
        if let Some(save) = self.session_saver.in_flight.take() {
            let result = wait_off_the_runtime(save.pending).await;
            self.finish_session_save_with_checkpoint(
                save.purpose,
                result,
                self.clock.now,
                save.pane_exit_snapshot,
            );
        }

        if !self.policy.persists_session() {
            self.session_saver.clear_deadline();
            return;
        }

        let Some(job) = self.capture_final_session_save_job() else {
            self.session_saver.clear_deadline();
            return;
        };
        let pending = self
            .session_saver
            .persister
            .submit(job, self.clock.wall_now);
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
            self.record_session_save_result(result, self.clock.now);
        }
        self.session_saver.clear_deadline();
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
    /// Whether a save is in flight.
    pub(crate) fn save_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Stands in for an autosave the persister is still running; it
    /// finishes when the test completes the returned handle.
    pub(crate) fn hold_test_save_in_flight(&mut self) -> shepr_mux::persist::SaveCompletion {
        let (completion, pending) = shepr_mux::persist::PendingSave::channel();
        self.in_flight = Some(InFlightSave {
            pending,
            purpose: SessionSavePurpose::Autosave,
            pane_exit_snapshot: None,
        });
        completion
    }
}

#[cfg(test)]
impl App {
    /// Blocks until the save in flight, if any, has finished, and records
    /// its outcome.
    pub(super) fn wait_for_session_save(&mut self) {
        if let Some(save) = self.session_saver.in_flight.take() {
            let result = save.pending.wait();
            self.finish_session_save_with_checkpoint(
                save.purpose,
                result,
                self.clock.now,
                save.pane_exit_snapshot,
            );
        }
    }

    pub(crate) fn save_session_now(&mut self) -> bool {
        self.wait_for_session_save();

        if !self.policy.persists_session() {
            self.session_saver.clear_deadline();
            return true;
        }

        let job = self.capture_session_save_job();
        let result = self
            .session_saver
            .persister
            .submit(job, self.clock.wall_now)
            .wait();
        let saved = result.is_ok();
        self.finish_session_save_with_checkpoint(
            SessionSavePurpose::Autosave,
            result,
            self.clock.now,
            None,
        );
        if saved {
            self.session_saver.clear_deadline();
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
                        self.take_pane_exit_checkpoint_ready();
                        break;
                    }
                    self.session_saver.critical_save_retry_deadline = None;
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
            self.session_saver.clear_deadline();
            return;
        }
        let Some(job) = self.capture_final_session_save_job() else {
            self.session_saver.clear_deadline();
            return;
        };
        let result = self
            .session_saver
            .persister
            .submit(job, self.clock.wall_now)
            .wait();
        self.finish_final_session_save(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &shepr_config::Config::default(),
            super::super::AppPolicy::Test,
            api_rx,
        )
    }

    #[test]
    fn repeated_pane_exit_checkpoint_failures_release_the_held_exit() {
        let mut app = test_app();
        app.policy = super::super::AppPolicy::Production;
        app.session_saver.pane_exit_checkpoint_requested = true;
        app.session_saver.pane_exit_checkpoint_generation = 1;
        let purpose = SessionSavePurpose::Checkpoint {
            host_shutdown_generation: None,
            pane_exit_generation: Some(1),
        };
        let now = Instant::now();
        let disk_full = || Err(std::io::Error::other("disk full"));

        for attempt in 1..CHECKPOINT_MAX_FAILURES {
            app.finish_session_save_with_checkpoint(purpose, disk_full(), now, None);
            assert!(
                !app.take_pane_exit_checkpoint_ready(),
                "failure {attempt} keeps holding the exit"
            );
            assert!(app.pane_exit_checkpoint_requested());
            assert!(!app.pane_exit_checkpoint_settled());
            assert!(
                app.session_saver.critical_save_retry_deadline.is_some(),
                "failure {attempt} schedules a retry"
            );
        }

        app.finish_session_save_with_checkpoint(purpose, disk_full(), now, None);
        assert!(
            app.take_pane_exit_checkpoint_ready(),
            "the last failure releases the held exit for removal"
        );
        assert!(!app.pane_exit_checkpoint_requested());
        assert!(app.session_saver.critical_save_retry_deadline.is_none());
        assert!(
            app.request_pane_exit_checkpoint().is_none(),
            "later exits are removed without waiting on a failing disk"
        );

        app.finish_session_save_with_checkpoint(SessionSavePurpose::Autosave, Ok(()), now, None);
        assert!(
            !app.pane_exit_checkpoint_settled(),
            "a save that succeeds again restores pre-exit checkpoints"
        );
        app.policy = super::super::AppPolicy::Test;
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
            !app.pane_exit_checkpoint_requested(),
            "the exit asks for no second checkpoint"
        );
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
            .pane_exit_checkpoint_snapshot
            .as_mut()
            .expect("saved checkpoint")
            .terminal_ids
            .clear();

        app.save_session_before_teardown_async().await;

        assert_eq!(saved_pane_counts(&app), vec![2]);
        assert!(app.preserves_pane_exit_checkpoint());
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
    #[test]
    fn a_restore_that_drops_a_workspace_backs_up_the_saved_session_before_the_first_save() {
        use crate::test_support::{AppPathsFixture as _, ValidatedConfigFixture as _};
        use shepr_mux::persist::snapshot::{
            DirectionSnapshot, LayoutSnapshot, PaneSnapshot, SessionSnapshot, WorkspaceSnapshot,
        };

        let scratch = crate::test_support::ScratchDir::new("dropped-workspace-backup");
        let paths = shepr_config::AppPaths::test_at(&scratch);
        let config = shepr_config::ValidatedConfig::test_from_config_with_paths(
            shepr_config::Config::default(),
            None,
            paths.clone(),
        );
        let data_dir = paths.data_dir().to_path_buf();
        let lease =
            shepr_mux::persist::DataDirLease::acquire(&data_dir).expect("test session lease");

        // A working directory that does not exist restores each pane without
        // starting a shell.
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

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::with_paths(
            &config,
            &paths,
            lease,
            super::super::AppPolicy::Production,
            api_rx,
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
        use crate::test_support::{AppPathsFixture as _, ValidatedConfigFixture as _};

        let scratch = crate::test_support::ScratchDir::new("unusable-session-notice");
        let paths = shepr_config::AppPaths::test_at(&scratch);
        let config = shepr_config::ValidatedConfig::test_from_config_with_paths(
            shepr_config::Config::default(),
            None,
            paths.clone(),
        );
        let data_dir = paths.data_dir().to_path_buf();
        let lease =
            shepr_mux::persist::DataDirLease::acquire(&data_dir).expect("test session lease");
        let original = b"{ this is not a session".to_vec();
        std::fs::write(data_dir.join("session.json"), &original).expect("test precondition");

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::with_paths(
            &config,
            &paths,
            lease,
            super::super::AppPolicy::Production,
            api_rx,
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
        use crate::test_support::{AppPathsFixture as _, ValidatedConfigFixture as _};

        let scratch = crate::test_support::ScratchDir::new("fresh-start-no-notice");
        let paths = shepr_config::AppPaths::test_at(&scratch);
        let config = shepr_config::ValidatedConfig::test_from_config_with_paths(
            shepr_config::Config::default(),
            None,
            paths.clone(),
        );
        let lease = shepr_mux::persist::DataDirLease::acquire(paths.data_dir())
            .expect("test session lease");
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::with_paths(
            &config,
            &paths,
            lease,
            super::super::AppPolicy::Production,
            api_rx,
            super::super::tests::test_clock(),
        );
        assert_eq!(app.restore_notice, None);
        app.policy = super::super::AppPolicy::Test;
    }
}
