//! When the session is saved, and what a save's outcome means for the loop.
//!
//! Saving itself belongs to the session persister
//! (`shepr_mux::persist::SessionPersister`), which owns the data directory
//! lease and the writer on a thread of its own. This side decides when to
//! save (debounced autosaves, pane-exit and host-shutdown checkpoints,
//! retries), captures what to save on the event loop, where only the cheap
//! part happens (the structural snapshot and a probe of each shell's cwd),
//! and hands the result to the persister.
//!
//! The loop learns that a save finished from the persister's completion
//! signal (the `save_finished` notification `App::open` hands the persister
//! and `AppOutputs` waits on), not by polling: it waits on the signal and
//! reaps the save when it fires. While a save is in flight no
//! save deadline is reported, since nothing can start before the save ends,
//! and its end wakes the loop to reconsider them.

use std::time::{Duration, Instant};

use super::App;
use crate::backoff::Backoff;
use crate::limits::{CHECKPOINT_MAX_FAILURES, CHECKPOINT_RETRY_MIN};

mod autosave;
mod exit_checkpoint;
mod host_checkpoint;
use autosave::Autosave;
use exit_checkpoint::PaneExitCheckpoint;
pub(crate) use host_checkpoint::HostCheckpointOutcome;
use host_checkpoint::HostShutdownCheckpoint;
use shepr_mux::persist::CapturedLayout;

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
    /// Whether this save answers the host-shutdown request. A bool, not an
    /// enum: it is the only bool in this private type, so a call site cannot
    /// swap it with another.
    host: bool,
}

/// The held pane-exit generation a checkpoint save answers. A mutation
/// observed after the capture voids the layout but not the generation: the
/// exit is still released when the save lands.
struct ExitTicket {
    generation: CheckpointGeneration,
    /// The captured layout; `None` when it could not be paired with terminal
    /// identities, or a session mutation was observed after the capture.
    layout: Option<Box<CapturedLayout>>,
}

/// The save `start_background_session_save` should start now.
enum NextSave {
    Autosave,
    Checkpoint {
        exit_generation: Option<CheckpointGeneration>,
        /// As `CheckpointTicket::host`.
        host: bool,
    },
}

/// Scheduling values belong to the saver, so fixtures can use virtual time
/// and a policy appropriate to the behavior they exercise.
#[derive(Clone, Copy)]
pub(crate) struct SavePolicyConfig {
    pub(crate) debounce: Duration,
    pub(crate) retry: Backoff,
    pub(crate) checkpoint_retry: Backoff,
    pub(crate) checkpoint_max_failures: u8,
}

impl Default for SavePolicyConfig {
    fn default() -> Self {
        Self {
            debounce: crate::limits::SESSION_SAVE_DEBOUNCE,
            retry: Backoff::new(
                crate::limits::SESSION_SAVE_RETRY_MIN,
                crate::limits::SESSION_SAVE_RETRY_MAX,
            ),
            checkpoint_retry: Backoff::new(CHECKPOINT_RETRY_MIN, Duration::MAX),
            checkpoint_max_failures: CHECKPOINT_MAX_FAILURES,
        }
    }
}

pub(crate) struct SessionSaver {
    config: SavePolicyConfig,
    policy: SavePolicy,
    autosave: Autosave,
    exit: PaneExitCheckpoint,
    host: HostShutdownCheckpoint,
    /// At most one save is in flight: a due save waits for it, so every
    /// capture reaches the persister after the one before it finished.
    in_flight: Option<InFlightSave>,
    persister: Option<shepr_mux::persist::SessionPersister>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveMode {
    Persisting,
    /// The persister refused work permanently for this boot.
    Stopped,
    /// The required backup of the session source could not be opened. The
    /// operator must fix its access permissions and restart the server.
    BlockedOnBackup,
}

/// Save admission and host-shutdown freezing are independent: stopping a
/// frozen writer must survive cancellation of the host shutdown warning.
#[derive(Clone, Copy)]
struct SavePolicy {
    mode: SaveMode,
    frozen: bool,
}

impl SavePolicy {
    fn new() -> Self {
        Self {
            mode: SaveMode::Persisting,
            frozen: false,
        }
    }

    fn allows_saves(self) -> bool {
        self.mode == SaveMode::Persisting && !self.frozen
    }

    /// A stopped saver takes the request to fail it immediately, so the
    /// lifecycle stops waiting. Frozen savers ignore new requests.
    fn takes_host_checkpoint(self) -> bool {
        !self.frozen
    }

    fn is_stopped(self) -> bool {
        self.mode == SaveMode::Stopped
    }

    fn is_blocked_on_backup(self) -> bool {
        self.mode == SaveMode::BlockedOnBackup
    }

    fn is_unavailable(self) -> bool {
        self.mode != SaveMode::Persisting
    }

    fn freeze(&mut self) {
        self.frozen = true;
    }

    fn thaw(&mut self) {
        self.frozen = false;
    }

    fn stop(&mut self) {
        self.mode = SaveMode::Stopped;
    }

    fn block_on_backup(&mut self) {
        self.mode = SaveMode::BlockedOnBackup;
    }
}

impl SessionSaver {
    /// The persister fires its own completion signal, which the app's outputs
    /// own; the saver keeps no copy.
    pub(crate) fn new(persister: shepr_mux::persist::SessionPersister) -> Self {
        Self::with_config(persister, SavePolicyConfig::default())
    }

    pub(crate) fn with_config(
        persister: shepr_mux::persist::SessionPersister,
        config: SavePolicyConfig,
    ) -> Self {
        Self {
            config,
            policy: SavePolicy::new(),
            autosave: Autosave::with_config(config.debounce, config.retry),
            exit: PaneExitCheckpoint::new(),
            host: HostShutdownCheckpoint::new(),
            in_flight: None,
            persister: Some(persister),
        }
    }

    /// Whether nothing may start now whatever is requested or due: a save is
    /// in flight (its end fires the persister's completion signal, which wakes
    /// the loop), or the host checkpoint finished unsaved and no pane exit is
    /// held (the lifecycle freezes saves once it takes that result), or this
    /// boot's persistence has stopped.
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
        self.next_save(now).is_some()
    }

    /// The save to start now, including a checkpoint with no retry delay.
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
            if self.policy.is_unavailable() {
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

    fn block_persistence_on_backup(&mut self) {
        self.policy.block_on_backup();
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
    /// Freezes the saver for a host shutdown.
    pub(crate) fn freeze_session_saves(&mut self) {
        self.session_saver.freeze();
    }

    /// Restores the saver's own pre-freeze policy.
    pub(crate) fn thaw_session_saves(&mut self) {
        self.session_saver.thaw();
    }

    /// Thaw after a cancelled host shutdown: the live session is dirty again.
    pub(crate) fn resume_session_saves_after_cancel(&mut self) {
        self.thaw_session_saves();
        self.state.mark_session_dirty();
    }

    /// The current AppState dirty bit is the authority for a saved exit layout;
    /// saver mutation notifications only schedule writes and invalidate caches.
    pub(super) fn preserves_pane_exit_checkpoint(&self) -> bool {
        self.session_saver.exit.preserved().is_some() && !self.state.session_dirty()
    }

    // A missing identity must never replace the protected layout with the
    // post-exit layout. Keep the durable checkpoint if refreshing it fails.
    fn capture_final_session_save_job(
        &self,
    ) -> Result<Option<shepr_mux::persist::PersistJob>, shepr_mux::persist::SaveError> {
        let Some(layout) = self
            .session_saver
            .exit
            .preserved()
            .filter(|_| !self.state.session_dirty())
        else {
            return self
                .capture_session_save()
                .map(|capture| Some(capture.into_job()));
        };
        let job = layout.recapture(&self.terminal_runtimes);
        if job.is_none() {
            tracing::warn!(
                "could not pair fresh cwd probes with the saved pane-exit layout; keeping the durable checkpoint"
            );
        }
        Ok(job)
    }

    /// Consumes the pure state mutation signal and applies persistence effects
    /// once for this loop pass. AppState mutations and App-owned mutations use
    /// the same flag, so a handler cannot schedule the same change twice.
    pub(crate) fn sync_session_save_schedule(&mut self) {
        // Keep mutations pending while frozen or stopped: consuming this signal
        // would incorrectly make a preserved checkpoint authoritative again.
        // An epoch is unnecessary while event replay is synchronous and this
        // signal is retained until persistence can observe the mutation.
        if self.session_saver.policy.allows_saves() && self.state.take_session_dirty() {
            self.session_saver.note_mutation(self.clock.now);
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
    /// persister refused a save it can never run, so later layout changes
    /// are not restored on the next start.
    pub(crate) fn session_saves_stopped(&self) -> bool {
        self.session_saver.policy.is_stopped()
    }

    /// Whether the saved source could not be opened for the backup required
    /// before a replacement. The server stops saving for this boot so the
    /// client can tell the operator to fix access and restart.
    pub(crate) fn session_saves_blocked_on_backup(&self) -> bool {
        self.session_saver.policy.is_blocked_on_backup()
    }

    pub(crate) fn session_saves_frozen(&self) -> bool {
        self.session_saver.policy.frozen
    }

    /// The one place a save's outcome is applied to the autosave backoff and
    /// both checkpoint machines.
    fn finish_session_save(
        &mut self,
        kind: SaveKind,
        result: Result<(), shepr_mux::persist::SaveError>,
    ) {
        let now = self.clock.now;
        let (save_kind, generation) = match &kind {
            SaveKind::Autosave => ("autosave", None),
            SaveKind::Checkpoint(ticket) => (
                if ticket.host {
                    "host_checkpoint"
                } else {
                    "pane_exit_checkpoint"
                },
                ticket.exit.as_ref().map(|exit| exit.generation.0),
            ),
        };
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
                                exit.layout.filter(|_| !self.state.session_dirty()),
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
            Err(error) if error.is_blocked_on_backup() => {
                tracing::error!(
                    event = "session.save.blocked", subsystem = "persist",
                    kind = save_kind, generation, directory = %self.paths.data_dir().display(),
                    error = %error,
                    "session saves are blocked because the existing session could not be opened for backup; fix access and restart the server"
                );
                if !self.session_saver.policy.is_blocked_on_backup() {
                    self.state.mark_shell_projection_dirty();
                }
                self.session_saver.block_persistence_on_backup();
            }
            Err(error) if !error.is_retryable() => {
                tracing::error!(
                    event = "session.save.stopped", subsystem = "persist",
                    kind = save_kind, generation, directory = %self.paths.data_dir().display(),
                    error = %error,
                    "session persistence failed permanently; disabling session saves for this boot"
                );
                if !self.session_saver.policy.is_stopped() {
                    // Every client's snapshot carries the stop, so the user
                    // learns that later layout changes will not be restored.
                    self.state.mark_shell_projection_dirty();
                }
                self.session_saver.stop_persistence();
            }
            Err(error) => {
                // A retryable write or capture failure re-arms the normal
                // retry; a checkpoint's own retry is its machine's, below. A
                // failed capture submitted nothing, so the last good file
                // stays. (A refusal or abandonment took the branch above.)
                if matches!(
                    &error,
                    shepr_mux::persist::SaveError::CaptureInconsistent { .. }
                ) {
                    // Capture failed before the mux writer received a job; this
                    // layer owns that diagnostic. Writer failures are logged
                    // by the mux, and only their retry policy is recorded here.
                    tracing::warn!(
                        event = "session.capture.failed", subsystem = "persist",
                        kind = save_kind, generation, directory = %self.paths.data_dir().display(),
                        error = %error, "session capture failed; keeping the previous session file"
                    );
                }
                let (failures, delay) = self.session_saver.autosave.record_failure(now);
                tracing::debug!(
                    event = "session.save.retry", subsystem = "persist",
                    kind = save_kind, generation, directory = %self.paths.data_dir().display(),
                    error = %error, failures, retry_ms = delay.as_millis(),
                    "session save retry scheduled"
                );
                if let SaveKind::Checkpoint(ticket) = kind {
                    if let Some(exit) = ticket.exit
                        && self.session_saver.exit.failed_with_config(
                            exit.generation,
                            now,
                            self.session_saver.config,
                        )
                    {
                        tracing::warn!(
                            event = "session.checkpoint.abandoned", subsystem = "persist",
                            kind = "pane_exit_checkpoint", generation = exit.generation.0,
                            directory = %self.paths.data_dir().display(),
                            failures = self.session_saver.config.checkpoint_max_failures,
                            "pane exit checkpoint failed repeatedly; removing exited panes without persisting their exit"
                        );
                    }
                    if ticket.host {
                        self.session_saver
                            .host
                            .failed_with_config(now, self.session_saver.config);
                    }
                }
            }
        }
    }

    /// Runs on the event loop, so it takes only what must be read here: the
    /// structural snapshot and a probe of each shell's cwd. No /proc file is
    /// read; reading the cwds is the persister's work.
    fn capture_session_save(
        &self,
    ) -> Result<shepr_mux::persist::SessionCapture, shepr_mux::persist::SaveError> {
        shepr_mux::persist::capture_job(
            &self.state.workspaces,
            &self.terminal_runtimes,
            self.paths.fallback_cwd(),
            self.state.host_terminal_theme(),
        )
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
                if self.state.take_session_dirty() {
                    self.session_saver.note_mutation(self.clock.now);
                }
                self.session_saver.autosave.clear();
                let capture = match self.capture_session_save() {
                    Ok(capture) => capture,
                    Err(error) => {
                        self.finish_session_save(
                            SaveKind::Checkpoint(CheckpointTicket {
                                exit: exit_generation.map(|generation| ExitTicket {
                                    generation,
                                    layout: None,
                                }),
                                host,
                            }),
                            Err(error),
                        );
                        return;
                    }
                };
                // Only a held pane exit keeps the layout it saves.
                let (job, exit) = match exit_generation {
                    Some(generation) => {
                        let (job, layout) = capture.into_job_with_layout();
                        let exit = ExitTicket {
                            generation,
                            layout: layout.map(Box::new),
                        };
                        (job, Some(exit))
                    }
                    None => (capture.into_job(), None),
                };
                let ticket = CheckpointTicket { exit, host };
                self.spawn_session_save(job, SaveKind::Checkpoint(ticket));
            }
            Some(NextSave::Autosave) => {
                self.session_saver.autosave.clear();
                match self.capture_session_save() {
                    Ok(capture) => {
                        self.spawn_session_save(capture.into_job(), SaveKind::Autosave);
                    }
                    Err(error) => self.finish_session_save(SaveKind::Autosave, Err(error)),
                }
            }
        }
    }

    fn spawn_session_save(&mut self, job: shepr_mux::persist::PersistJob, kind: SaveKind) {
        let Some(persister) = self.session_saver.persister.as_mut() else {
            self.finish_session_save(kind, Err(shepr_mux::persist::SaveError::Abandoned));
            return;
        };
        let pending = persister.submit(job, self.clock.wall_now);
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
        let generation = self
            .session_saver
            .exit
            .request(self.state.session_dirty())?;
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

    pub(crate) fn take_host_shutdown_checkpoint_result(&mut self) -> Option<HostCheckpointOutcome> {
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
                self.state.take_session_dirty();
            }
            if self.session_saver.policy.allows_saves() {
                self.session_saver.autosave.schedule(self.clock.now);
            }
        }
    }

    fn submit_final_session_save(
        &mut self,
    ) -> Result<Option<shepr_mux::persist::PendingSave>, shepr_mux::persist::SaveError> {
        if !self.session_saver.policy.allows_saves() {
            self.session_saver.autosave.clear();
            return Ok(None);
        }

        let Some(job) = self.capture_final_session_save_job()? else {
            self.session_saver.autosave.clear();
            return Ok(None);
        };
        let Some(persister) = self.session_saver.persister.as_mut() else {
            return Err(shepr_mux::persist::SaveError::Abandoned);
        };
        Ok(Some(persister.submit(job, self.clock.wall_now)))
    }

    fn finish_final_session_save(
        &mut self,
        result: Result<(), std::io::Error>,
    ) -> Result<(), std::io::Error> {
        // No autosave retry or projection is useful once the loop has ended.
        // Clear the deadline here even if retirement is delayed by teardown;
        // callers of the final save observe the same terminal scheduling state.
        // Keep the previous atomic save on failure and report an unclean exit.
        self.session_saver.autosave.clear();
        if let Err(error) = &result {
            tracing::error!(
                event = "session.save.final_failure", subsystem = "persist", kind = "final",
                directory = %self.paths.data_dir().display(),
                %error,
                "final session save failed"
            );
        }
        result
    }

    /// The final save of this boot. During a host
    /// shutdown saving is frozen, so this writes nothing and the checkpoint
    /// taken on the warning stands. A signal quit's instant adopts checkpoint
    /// candidates first: after a signal the panes' deaths were left
    /// unprocessed, so a pane whose agent died from the same kill just before
    /// it gets that identity back here.
    pub(crate) async fn save_session_for_exit(
        &mut self,
        signal_quit_at: Option<Instant>,
    ) -> Result<(), std::io::Error> {
        if let Some(signaled_at) = signal_quit_at {
            self.state
                .adopt_checkpoint_candidates_for_shutdown(signaled_at);
        }
        self.save_session_before_teardown_async().await
    }

    pub(crate) async fn save_session_before_teardown_async(
        &mut self,
    ) -> Result<(), std::io::Error> {
        if let Some(save) = self.session_saver.in_flight.take() {
            let result = wait_off_the_runtime(save.pending).await;
            self.finish_session_save(save.kind, result);
        }

        let pending = match self.submit_final_session_save() {
            Ok(Some(pending)) => pending,
            Ok(None) if self.session_saver.policy.is_unavailable() => {
                return self.finish_final_session_save(Err(std::io::Error::other(
                    "session persistence was blocked before the final save",
                )));
            }
            Ok(None) => return Ok(()),
            Err(error) => {
                return self.finish_final_session_save(Err(std::io::Error::other(error)));
            }
        };
        // Keep a blocking task panic or cancellation as the error source.
        let result = match tokio::task::spawn_blocking(move || pending.wait()).await {
            Ok(result) => result.map_err(std::io::Error::other),
            Err(error) => Err(std::io::Error::other(error)),
        };
        self.finish_final_session_save(result)
    }

    /// Normal async shutdown moves the writer's join off the runtime. Drop
    /// retains the synchronous backstop for errors and unwinding.
    pub(crate) async fn retire_session_writer_async(&mut self) -> Result<(), std::io::Error> {
        if let Some(save) = self.session_saver.in_flight.take() {
            let result = wait_off_the_runtime(save.pending).await;
            self.finish_session_save(save.kind, result);
        }
        self.session_saver.autosave.clear();
        if let Some(mut persister) = self.session_saver.persister.take() {
            let persister = tokio::task::spawn_blocking(move || {
                persister.retire();
                persister
            })
            .await
            .map_err(std::io::Error::other)?;
            self.session_saver.persister = Some(persister);
        }
        Ok(())
    }

    /// Ends persistence for this server: the save still in flight finishes
    /// (its failure is logged like any other save's; the retry it schedules
    /// is moot, the deadline is cleared below), then the persister releases
    /// the data directory lease. A `shepr stop` waits for that release.
    pub(crate) fn retire_session_writer(&mut self) {
        if let Some(save) = self.session_saver.in_flight.take() {
            let result = save.pending.wait();
            self.finish_session_save(save.kind, result);
        }
        self.session_saver.autosave.clear();
        if let Some(persister) = self.session_saver.persister.as_mut() {
            persister.retire();
        }
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

/// Retry delay of a failed pane-exit or host-shutdown checkpoint after
/// `failures_before` earlier failures of the same checkpoint, under the
/// default save policy (`SavePolicyConfig::default`).
#[cfg(test)]
fn checkpoint_retry_delay(failures_before: u8) -> Duration {
    SavePolicyConfig::default()
        .checkpoint_retry
        .delay_after(u32::from(failures_before))
}

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

    /// Holds a host checkpoint until the test delivers its completion, so
    /// the event loop can be exercised after it has gone idle.
    pub(crate) fn hold_test_host_checkpoint(&mut self) -> shepr_mux::persist::SaveCompletion {
        self.host.request();
        self.hold_test_kind(SaveKind::Checkpoint(CheckpointTicket {
            exit: None,
            host: true,
        }))
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

    /// The layout a pane-exit checkpoint preserved, for tests that alter it.
    fn preserved_layout_mut(&mut self) -> Option<&mut CapturedLayout> {
        self.exit.preserved_mut()
    }
}

#[cfg(test)]
impl App {
    /// Retires the persister and starts a fresh one on the same scratch data
    /// directory, firing `save_finished`, with saves admitted again. Test
    /// apps already persist through their outputs' signal from `App::new`;
    /// this only matters to a test that stopped or retired the saver.
    /// `save_finished` is the signal
    /// whoever owns this app's outputs waits on (`TestApp::persist`,
    /// `HeadlessServer::persist_for_test`).
    pub(crate) fn persist_with_signal(
        &mut self,
        save_finished: std::sync::Arc<tokio::sync::Notify>,
    ) {
        if let Some(persister) = self.session_saver.persister.as_mut() {
            persister.retire();
        }
        let lease = shepr_mux::persist::DataDirLease::acquire(self.paths.data_dir())
            .expect("the test data directory lease is free");
        self.session_saver.persister = Some(shepr_mux::persist::SessionPersister::spawn(
            lease,
            shepr_mux::persist::SessionBackupPolicy::NoBackupNeeded,
            save_finished,
        ));
        self.session_saver.policy.mode = SaveMode::Persisting;
    }

    /// Drives the production reap until the current save completes.
    /// Synchronous event fixtures retain this driver because converting their
    /// callers to async would cross the app and pane-lifecycle test scopes.
    /// A wedged writer fails the test at `SESSION_WRITE_TEST_BOUND` rather
    /// than hanging it; that bound is far above any healthy write.
    pub(super) fn wait_for_session_save(&mut self) {
        // Keep the synchronous fixture wait as a driver of the production reap,
        // rather than a second implementation of save outcome handling.
        let started = std::time::Instant::now();
        while self.session_saver.save_in_flight() {
            if !self.reap_finished_session_save() {
                assert!(
                    started.elapsed() < crate::test_support::SESSION_WRITE_TEST_BOUND,
                    "the session writer did not finish a save"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::SESSION_SAVE_RETRY_MIN;

    fn test_app() -> crate::app::TestApp {
        App::new(&shepr_config::ServerConfig::default())
    }

    #[test]
    fn injected_checkpoint_policy_controls_delay_and_failure_limit() {
        let mut app = test_app();
        let config = SavePolicyConfig {
            debounce: Duration::ZERO,
            retry: Backoff::new(Duration::from_millis(2), Duration::from_millis(8)),
            checkpoint_retry: Backoff::new(Duration::from_millis(11), Duration::from_millis(11)),
            checkpoint_max_failures: 2,
        };
        let persister = app
            .session_saver
            .persister
            .take()
            .expect("fixture persister");
        app.session_saver = SessionSaver::with_config(persister, config);
        app.state.mark_session_dirty();
        app.sync_session_save_schedule();
        assert_eq!(app.session_saver.deadline(), Some(app.clock.now));
        app.session_saver.host.request();
        app.finish_session_save(
            SaveKind::Checkpoint(CheckpointTicket {
                exit: None,
                host: true,
            }),
            disk_full(),
        );
        assert_eq!(
            app.session_saver.host.retry_at(),
            Some(app.clock.now + config.checkpoint_retry.delay_after(0))
        );
        app.finish_session_save(
            SaveKind::Checkpoint(CheckpointTicket {
                exit: None,
                host: true,
            }),
            disk_full(),
        );
        assert!(app.session_saver.host.finished_unsaved());
    }

    #[test]
    fn frozen_saves_retain_mutations_until_thawed() {
        let mut app = test_app();
        app.freeze_session_saves();
        app.state.mark_session_dirty();
        app.sync_session_save_schedule();
        assert!(app.state.session_dirty());
        assert!(app.session_saver.autosave_deadline().is_none());
        app.thaw_session_saves();
        app.sync_session_save_schedule();
        assert!(!app.state.session_dirty());
        assert!(app.session_saver.autosave_deadline().is_some());
    }

    #[test]
    fn a_frozen_mutation_invalidates_the_preserved_exit_layout() {
        let (mut app, exiting, _) = two_pane_app("frozen mutation");
        let death = interrupted_death(&app, exiting);
        let _ = app.prepare_pane_exit(death);
        app.wait_for_session_save();
        assert!(app.preserves_pane_exit_checkpoint());
        app.freeze_session_saves();
        app.state.mark_session_dirty();
        app.sync_session_save_schedule();
        assert!(!app.preserves_pane_exit_checkpoint());
        app.sync_session_save_schedule();
        assert!(!app.preserves_pane_exit_checkpoint());
    }

    #[tokio::test]
    async fn async_retirement_releases_the_data_directory_lease() {
        let mut app = test_app();
        let completion = app.session_saver.hold_test_kind(SaveKind::Autosave);
        let responder = tokio::spawn(async move {
            tokio::task::yield_now().await;
            completion.complete(Ok(()));
        });
        app.retire_session_writer_async()
            .await
            .expect("retired writer");
        responder.await.expect("runtime served the save completion");
        assert!(shepr_mux::persist::DataDirLease::acquire(app.paths.data_dir()).is_ok());
    }

    #[tokio::test]
    async fn a_final_save_failure_is_returned_without_arming_an_autosave_retry() {
        let mut app = test_app();
        // Retirement is a real permanent refusal, without a test-only mode.
        if let Some(persister) = app.session_saver.persister.as_mut() {
            persister.retire();
        }
        let error = app
            .save_session_before_teardown_async()
            .await
            .expect_err("the retired persister cannot save");
        assert!(error.get_ref().is_some());
        assert!(app.session_saver.autosave_deadline().is_none());
        assert!(!app.session_saver.policy.is_stopped());
    }

    #[test]
    fn stopping_a_frozen_saver_survives_thaw() {
        let mut policy = SavePolicy::new();
        assert!(policy.allows_saves());
        policy.freeze();
        assert!(!policy.allows_saves());
        assert!(!policy.takes_host_checkpoint());
        policy.stop();
        policy.thaw();
        assert!(policy.is_stopped());
        assert!(!policy.allows_saves());
        assert!(policy.takes_host_checkpoint());
    }

    /// A production-policy app with one workspace of two panes, returning the
    /// pane that will exit.
    fn two_pane_app(
        name: &str,
    ) -> (
        crate::app::TestApp,
        shepr_core::layout::PaneId,
        shepr_core::layout::PaneId,
    ) {
        use crate::test_support::WorkspaceFixture as _;
        let mut app = test_app();
        app.persist();
        let mut workspace = shepr_mux::workspace::Workspace::test_new(name);
        let exiting = workspace.tree().root();
        let staying = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        app.state.test_set_workspaces(vec![workspace]);
        app.state.seed_bookmark_index(Some(0));
        app.insert_idle_test_runtime(exiting);
        app.insert_idle_test_runtime(staying);
        (app, exiting, staying)
    }

    /// A signalled exit of the pane, as its runtime reports it.
    fn interrupted_exit(app: &App, pane_id: shepr_core::layout::PaneId) -> AppEvent {
        app.from_pane_runtime(
            pane_id,
            shepr_mux::events::RuntimeEvent::PaneDied {
                ending: shepr_mux::pane::PaneEnding::new(shepr_mux::pane::PaneEndReason::Signalled),
                ended_at: std::time::Instant::now(),
            },
        )
    }

    /// The same signalled exit, admitted: the input `prepare_pane_exit` takes.
    fn interrupted_death(app: &App, pane_id: shepr_core::layout::PaneId) -> crate::app::PaneDeath {
        app.test_pane_death(
            pane_id,
            shepr_mux::pane::PaneEndReason::Signalled,
            std::time::Instant::now(),
        )
    }

    fn saved_pane_counts(app: &App) -> Vec<usize> {
        let saved = std::fs::read_to_string(shepr_mux::persist::session_path(app.paths.data_dir()))
            .expect("read the session file");
        shepr_mux::persist::schema::parse_session_file(&saved)
            .expect("parse the session file")
            .workspaces
            .iter()
            .map(|workspace| workspace.layout.panes().len())
            .collect()
    }

    #[test]
    fn a_held_pane_exit_settles_on_its_checkpoint_although_the_session_changed_meanwhile() {
        let (mut app, exiting, _) = two_pane_app("held");

        let death = interrupted_death(&app, exiting);
        let generation = app
            .prepare_pane_exit(death)
            .prepared
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

        let death = interrupted_death(&app, exiting);
        let generation = app
            .prepare_pane_exit(death)
            .prepared
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
        server.install_test_app(app);

        server.handle_test_runtime_exit_and_replay(interrupted_exit(&server.app, exiting));
        assert_eq!(saved_pane_counts(&server.app), vec![2]);
        let death = interrupted_death(&server.app, staying);
        assert!(
            server.app.prepare_pane_exit(death).prepared.is_settled(),
            "the checkpoint on disk already holds the second pane"
        );
    }

    #[tokio::test]
    async fn the_final_save_rewrites_the_checkpoint_layout_instead_of_skipping_it() {
        let (app, exiting, _) = two_pane_app("final");
        let mut server = crate::server::headless::tests::test_headless_server();
        server.install_test_app(app);
        server.handle_test_runtime_exit_and_replay(interrupted_exit(&server.app, exiting));
        assert_eq!(server.app.state.ws(0).tree().len(), 1);
        // A final save that skipped would leave no session file behind.
        std::fs::remove_file(shepr_mux::persist::session_path(
            server.app.paths.data_dir(),
        ))
        .expect("remove the checkpoint");

        server
            .app
            .save_session_before_teardown_async()
            .await
            .expect("final save");

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
        server.install_test_app(app);
        server.handle_test_runtime_exit_and_replay(interrupted_exit(&server.app, exiting));
        let layout = server
            .app
            .session_saver
            .preserved_layout_mut()
            .expect("saved checkpoint");
        *layout = CapturedLayout::new(layout.snapshot().clone(), std::collections::HashMap::new());

        server
            .app
            .save_session_before_teardown_async()
            .await
            .expect("final save");

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
        app.persist();
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
        app.persist();
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
    fn an_unreadable_backup_source_blocks_saves_and_projects_the_condition() {
        let mut app = test_app();
        app.persist();
        let generation = app.session_saver.exit.request(true).expect("held");
        let projection_before = app.state.shell_projection_revision;

        app.finish_session_save(
            exit_kind(generation),
            Err(shepr_mux::persist::SaveError::BlockedOnBackup(
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied"),
            )),
        );

        assert!(app.session_saves_blocked_on_backup());
        assert!(!app.session_saves_stopped());
        assert_ne!(app.state.shell_projection_revision, projection_before);
        assert!(app.pane_exit_checkpoint_generation_settled(generation));
        assert_eq!(app.session_saver.deadline(), None);
        assert!(!app.session_saver.policy.allows_saves());
    }

    /// A capture inconsistency submitted nothing, so the last good file is
    /// untouched; the saver retries on its backoff instead of stopping for
    /// the boot, and a later consistent capture saves again.
    #[test]
    fn a_capture_inconsistency_is_retried_rather_than_stopping_saves() {
        let mut app = test_app();
        app.persist();
        let generation = app.session_saver.exit.request(true).expect("held");

        app.finish_session_save(
            exit_kind(generation),
            Err(shepr_mux::persist::SaveError::CaptureInconsistent {
                workspace: "w1".into(),
                detail: "layout and pane records disagree",
            }),
        );

        assert!(!app.session_saves_stopped());
        assert!(!app.session_saves_blocked_on_backup());
        assert!(app.session_saver.policy.allows_saves());
        assert!(!app.pane_exit_checkpoint_generation_settled(generation));
        assert!(app.session_saver.exit.retry_at().is_some());
        app.wait_for_session_save();
    }

    #[test]
    fn a_checkpoint_without_a_retry_is_due_without_an_autosave_deadline() {
        let mut app = test_app();
        let now = app.clock.now;
        assert!(!app.session_saver.is_due(now));
        app.session_saver.host.request();
        assert_eq!(app.session_saver.deadline(), None);
        assert!(app.session_saver.is_due(now));
        app.service_session_saves(now);
        assert!(app.session_saver.save_in_flight());
        assert!(!app.session_saver.is_due(now));
        app.wait_for_session_save();
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
        let later = now + checkpoint_retry_delay(1);
        assert_eq!(
            app.session_saver.exit.retry_at(),
            Some(now + checkpoint_retry_delay(0))
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
        let layout = Box::new(CapturedLayout::new(
            shepr_mux::persist::schema::SessionSnapshot {
                version: shepr_mux::persist::schema::SNAPSHOT_VERSION,
                host_theme: Default::default(),
                workspaces: vec![],
                active: None,
            },
            std::collections::HashMap::new(),
        ));
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
        let now = app.clock.now;
        app.session_saver.note_mutation(now);
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
        let now = app.clock.now;
        app.session_saver.autosave.schedule(now);
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
            Some(app.clock.now + checkpoint_retry_delay(0))
        );
        assert_eq!(
            app.session_saver.autosave_deadline(),
            Some(
                app.clock.now
                    + Backoff::new(
                        SESSION_SAVE_RETRY_MIN,
                        crate::limits::SESSION_SAVE_RETRY_MAX
                    )
                    .delay_after(6)
            )
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
        assert_eq!(
            app.take_host_shutdown_checkpoint_result(),
            Some(HostCheckpointOutcome::Saved)
        );
    }

    #[test]
    fn a_host_shutdown_checkpoint_saves_the_live_layout_and_finishes() {
        let (mut app, _, _) = two_pane_app("host-live");
        app.request_host_shutdown_checkpoint();
        app.wait_for_session_save();
        assert!(app.host_shutdown_checkpoint_result_ready());
        assert_eq!(
            app.take_host_shutdown_checkpoint_result(),
            Some(HostCheckpointOutcome::Saved)
        );
        assert_eq!(app.take_host_shutdown_checkpoint_result(), None);
        assert_eq!(saved_pane_counts(&app), vec![2]);
    }

    #[test]
    fn a_host_checkpoint_supersedes_a_preserved_pane_exit_layout() {
        let (app, exiting, _) = two_pane_app("host-supersedes");
        let mut server = crate::server::headless::tests::test_headless_server();
        server.install_test_app(app);
        server.handle_test_runtime_exit_and_replay(interrupted_exit(&server.app, exiting));
        assert!(server.app.preserves_pane_exit_checkpoint());
        server.app.request_host_shutdown_checkpoint();
        server.app.wait_for_session_save();
        assert_eq!(saved_pane_counts(&server.app), vec![1]);
        assert!(!server.app.preserves_pane_exit_checkpoint());
        assert_eq!(
            server.app.take_host_shutdown_checkpoint_result(),
            Some(HostCheckpointOutcome::Saved)
        );
    }

    #[test]
    fn a_mutation_pending_when_a_checkpoint_starts_discards_the_preserved_layout() {
        let (app, exiting, _) = two_pane_app("pending-mutation");
        let mut server = crate::server::headless::tests::test_headless_server();
        server.install_test_app(app);
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
        assert_eq!(
            app.take_host_shutdown_checkpoint_result(),
            Some(HostCheckpointOutcome::Saved)
        );
        assert_eq!(saved_pane_counts(&app), vec![2]);
    }

    #[tokio::test]
    async fn two_exits_held_by_one_checkpoint_keep_the_pre_exit_layout_through_teardown() {
        let (mut app, first, second) = two_pane_app("overlapping");
        app.state
            .test_split_workspace(0, shepr_core::layout::Direction::Horizontal);
        let first_death = interrupted_death(&app, first);
        let first_prepared = app.prepare_pane_exit(first_death).prepared;
        let second_death = interrupted_death(&app, second);
        let second_prepared = app.prepare_pane_exit(second_death).prepared;
        let first_generation = first_prepared.held_generation().expect("first held");
        let second_generation = second_prepared.held_generation().expect("second held");
        app.wait_for_session_save();
        assert!(app.pane_exit_checkpoint_generation_settled(first_generation));
        assert!(app.pane_exit_checkpoint_generation_settled(second_generation));
        assert!(!app.session_saver.exit.is_requested());
        assert!(app.handle_prepared_pane_exit(&first_prepared));
        assert!(app.handle_prepared_pane_exit(&second_prepared));
        // Both exits were applied, so the three panes saved below are the
        // preserved pre-exit layout, not the live one.
        assert_eq!(app.state.ws(0).tree().len(), 1);
        app.sync_session_save_schedule();
        app.save_session_before_teardown_async()
            .await
            .expect("final save");
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

    /// `App::open` builds the app from the session `open_session` opened: its
    /// restored workspaces, its restore notice and the persister that saves
    /// it. The open sequence's own decisions (what is restored, what is
    /// reported, what is backed up) are tested with it in shepr-mux.
    #[tokio::test]
    async fn the_app_opens_on_the_restored_session_and_saves_through_its_persister() {
        use crate::test_support::{AppPathsFixture as _, ValidatedServerConfigFixture as _};
        use shepr_mux::persist::schema::{
            DirectionSnapshot, LayoutSnapshot, PaneSnapshot, SessionSnapshot, WorkspaceSnapshot,
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
        let number = |value: usize| shepr_protocol::PanePublicNumber::new(value).expect("number");
        let pane = |public_number: usize| PaneSnapshot {
            cwd: shepr_core::absolute_path::AbsolutePath::new(scratch.join("missing-cwd"))
                .expect("a scratch path is absolute"),
            public_number: number(public_number),
            label: None,
            agent_session: None,
            unusable_agent_session: None,
        };
        let workspace = |id: &str, name: &str, layout: LayoutSnapshot, next: usize| {
            let first = layout.panes()[0].public_number;
            WorkspaceSnapshot {
                id: id.parse().expect("workspace id"),
                name: shepr_mux::terminal::Label::new(name).expect("test workspace name"),
                layout,
                next_public_pane_number: number(next),
                zoomed: false,
                focused: first,
                root_pane: first,
            }
        };
        // A saved split ratio out of range refuses the whole file at decode,
        // so the workspace-level defect here is two panes sharing one public
        // number, which drops only that workspace.
        let colliding = workspace(
            "w2",
            "colliding numbers",
            LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: shepr_core::layout::SplitRatio::EVEN,
                first: Box::new(LayoutSnapshot::Pane(pane(1))),
                second: Box::new(LayoutSnapshot::Pane(pane(1))),
            },
            2,
        );
        let snapshot = SessionSnapshot {
            version: shepr_mux::persist::schema::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![
                workspace("w1", "healthy", LayoutSnapshot::Pane(pane(1)), 2),
                colliding,
            ],
            active: Some(0),
        };
        let original = serde_json::to_vec(&snapshot).expect("encode the saved session");
        std::fs::write(shepr_mux::persist::session_path(&data_dir), &original)
            .expect("test precondition");

        let (mut app, _outputs) =
            App::open(&config, &paths, lease, super::super::tests::test_clock());
        assert_eq!(
            app.state
                .workspaces
                .iter()
                .map(shepr_mux::workspace::Workspace::name)
                .collect::<Vec<_>>(),
            vec!["healthy"],
            "the saved session loaded and only the invalid workspace was dropped"
        );
        let backups = paths.session_backup_directory();
        // The notice every client of this boot is sent.
        assert_eq!(
            app.restore_notice,
            Some(shepr_protocol::SessionRestoreNotice {
                loss: shepr_protocol::SessionRestoreLoss::Damaged(
                    shepr_protocol::SessionRestoreDamage {
                        dropped_workspaces: 1,
                        ..Default::default()
                    }
                ),
                backup_dir: backups.clone().into(),
            })
        );

        // The persister the app saves through took the backup decision.
        app.save_session_before_teardown_async()
            .await
            .expect("first save");
        assert_eq!(directory_files(&backups), vec![original]);
    }

    /// Autosave scheduling and checkpoints, run on the app fixture
    /// `app::tests` shares (it sets the test shell).
    mod autosave_and_checkpoints {
        use crate::app::tests::test_app;
        use crate::app::*;
        use crate::limits::SESSION_SAVE_DEBOUNCE;
        use crate::test_support::*;
        use shepr_agent::{Agent, AgentState};
        use shepr_mux::workspace::Workspace;

        /// The pane's exit as its current runtime reports it.
        fn runtime_pane_exit(
            app: &App,
            pane_id: shepr_core::layout::PaneId,
            reason: shepr_mux::pane::PaneEndReason,
            ended_at: Instant,
        ) -> AppEvent {
            app.from_pane_runtime(
                pane_id,
                shepr_mux::events::RuntimeEvent::PaneDied {
                    ending: shepr_mux::pane::PaneEnding::new(reason),
                    ended_at,
                },
            )
        }

        /// The detector's exit report for the pane's agent, then its withdrawal,
        /// as the pane's current runtime publishes them.
        fn release_agent(app: &mut App, pane_id: shepr_core::layout::PaneId) {
            for (agent, process_exited) in [(Some(Agent::Claude), true), (None, false)] {
                let event = app.from_pane_runtime(
                    pane_id,
                    shepr_mux::events::RuntimeEvent::StateChanged {
                        agent,
                        detection: shepr_detect::Detection::new(AgentState::Idle, false),
                        process_exited,
                        observed_at: app.clock.now,
                    },
                );
                app.handle_internal_event(event);
            }
        }

        #[test]
        fn session_dirty_flag_schedules_debounced_save() {
            let mut app = test_app();
            app.persist();
            let sample = AppClock {
                now: app.clock.now + Duration::from_secs(42),
                wall_now: app.clock.wall_now,
            };
            app.set_clock(sample);
            app.state.session_dirty = true;

            app.sync_session_save_schedule();

            assert!(!app.state.session_dirty);
            assert_eq!(
                app.session_saver.autosave_deadline(),
                Some(sample.now + SESSION_SAVE_DEBOUNCE)
            );
        }

        #[test]
        fn due_session_save_starts_background_writer() {
            let mut app = test_app();
            app.persist();
            app.state
                .test_set_workspaces(vec![Workspace::test_new("autosave")]);
            let now = app.clock.now;
            app.session_saver.set_autosave_deadline(Some(now));

            app.start_background_session_save();

            assert!(app.session_saver.save_in_flight());
            assert!(app.session_saver.autosave_deadline().is_none());
            app.wait_for_session_save();
            assert!(
                shepr_mux::persist::session_path(app.paths.data_dir())
                    .try_exists()
                    .expect("stat session file")
            );
        }

        #[test]
        fn background_session_save_reschedules_when_writer_is_busy() {
            let mut app = test_app();
            app.persist();
            let release = app.session_saver.hold_test_save_in_flight();
            let now = app.clock.now;
            app.session_saver.set_autosave_deadline(Some(now));

            app.start_background_session_save();

            assert!(app.session_saver.save_in_flight());
            assert_eq!(app.session_saver.deadline(), None);

            release.complete(Ok(()));
            app.wait_for_session_save();
            assert_eq!(app.session_saver.deadline(), Some(app.clock.now));
            app.start_background_session_save();
            assert!(app.session_saver.save_in_flight());
            app.wait_for_session_save();
        }

        #[tokio::test]
        async fn final_session_save_joins_background_writer_before_returning() {
            let mut app = test_app();
            let release = app.session_saver.hold_test_save_in_flight();
            let mut final_save = Box::pin(app.save_session_before_teardown_async());
            // Poll the actual final-save future while completion is withheld.
            // The failure below also proves the joined outcome is applied
            // before the final save admits a new capture.
            assert!(
                std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(
                        std::future::Future::poll(final_save.as_mut(), cx).is_pending(),
                    )
                })
                .await
            );
            release.complete(Err(shepr_mux::persist::SaveError::Abandoned));
            final_save
                .await
                .expect_err("the joined background failure stops final-save admission");
            assert!(app.session_saves_stopped());
            assert!(!app.session_saver.save_in_flight());
        }

        #[tokio::test]
        async fn pane_exit_checkpoint_survives_automatic_workspace_creation_on_shutdown() {
            let mut server = crate::server::headless::tests::test_headless_server();
            server.install_test_app(test_app());
            server.persist_for_test();
            let mut workspace = Workspace::test_new("preserved");
            let first_pane = workspace.tree().root();
            let second_pane = workspace.test_split(shepr_core::layout::Direction::Horizontal);
            server.app.state.test_set_workspaces(vec![workspace]);
            server.app.state.seed_bookmark_index(Some(0));
            server.app.insert_idle_test_runtime(first_pane);
            server.app.insert_idle_test_runtime(second_pane);

            server.handle_test_runtime_exit_and_replay(runtime_pane_exit(
                &server.app,
                first_pane,
                shepr_mux::pane::PaneEndReason::Signalled,
                std::time::Instant::now(),
            ));
            server.handle_test_runtime_exit_and_replay(runtime_pane_exit(
                &server.app,
                second_pane,
                shepr_mux::pane::PaneEndReason::Signalled,
                std::time::Instant::now(),
            ));
            assert!(server.app.state.workspaces.is_empty());
            let geometry = server.app.headless_spawn_geometry();
            assert_eq!(
                server.app.create_default_workspace(geometry),
                crate::app::DefaultWorkspace::Created
            );

            server
                .app
                .save_session_before_teardown_async()
                .await
                .expect("final save");
            server.app.retire_session_writer();

            let lease = shepr_mux::persist::DataDirLease::acquire(server.app.paths.data_dir())
                .expect("test lease");
            let snapshot = shepr_mux::persist::load(&lease)
                .into_snapshot()
                .expect("checkpointed session should survive");
            assert_eq!(snapshot.workspaces.len(), 1);
            assert_eq!(snapshot.workspaces[0].layout.panes().len(), 2);
        }

        #[tokio::test]
        async fn detector_release_before_pane_exit_keeps_checkpoint_resume_identity() {
            use shepr_agent::resume::{AgentSessionRef, PersistedAgentSession};
            let _env = crate::test_support::IsolatedEnv::new();
            let mut server = crate::server::headless::tests::test_headless_server();
            server.install_test_app(test_app());
            let geometry = server.app.headless_spawn_geometry();
            assert_eq!(
                server.app.create_default_workspace(geometry),
                crate::app::DefaultWorkspace::Created
            );
            let pane_id = server.app.state.ws(0).tree().root();
            // Let the real child exit, but keep its PaneDied queued. This exercises
            // the gap where the detector release can reach the app first.
            tokio::time::timeout(Duration::from_secs(5), async {
                while !server
                    .app
                    .terminal_runtimes
                    .get(&pane_id)
                    .expect("runtime")
                    .child_has_exited()
                {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("pane child exits");
            let session = PersistedAgentSession::from_report(
                "shepr:claude",
                AgentSessionRef::id("checkpoint-resume").expect("session id"),
            )
            .expect("official session");
            let now = server.app.clock.now;
            let terminal = server.app.state.terminal_mut(pane_id);
            terminal
                .ownership_mut()
                .set_detected_agent_process_at(Agent::Claude, now);
            terminal
                .ownership_mut()
                .set_persisted_agent_session(session.clone());
            // Delivered from the live runtime, so admission passes them and only
            // the exited child decides that they are ignored.
            release_agent(&mut server.app, pane_id);
            assert_eq!(
                server
                    .app
                    .state
                    .terminal(pane_id)
                    .expect("terminal")
                    .ownership()
                    .detected_agent(),
                Some(Agent::Claude),
            );
            assert_eq!(
                server
                    .app
                    .state
                    .terminal(pane_id)
                    .expect("terminal")
                    .ownership()
                    .current_session_identity_for_persistence(),
                Some(session.clone()),
            );
            server.persist_for_test();
            server.app.state.mark_session_dirty();
            server.handle_test_runtime_exit_and_replay(runtime_pane_exit(
                &server.app,
                pane_id,
                shepr_mux::pane::PaneEndReason::Signalled,
                std::time::Instant::now(),
            ));
            server
                .app
                .save_session_before_teardown_async()
                .await
                .expect("final save");
            server.app.retire_session_writer();
            let lease = shepr_mux::persist::DataDirLease::acquire(server.app.paths.data_dir())
                .expect("test lease");
            let snapshot = shepr_mux::persist::load(&lease)
                .into_snapshot()
                .expect("saved checkpoint");
            let saved = snapshot.workspaces[0].layout.panes()[0]
                .agent_session
                .as_ref()
                .expect("saved resume identity");
            assert_eq!(saved.source(), session.source());
            assert_eq!(saved.agent(), session.agent());
            assert_eq!(saved.session_ref(), session.session_ref());
        }

        /// A pane with a live agent session, ready to have its agent released by
        /// the detector while its shell still runs. The pane's runtime has no
        /// child process, so its shell never exits on its own: a spawned child
        /// that exits before the release is delivered would make the app ignore
        /// the release as detector evidence from an ended pane.
        fn app_with_agent_session() -> (
            TestApp,
            shepr_core::layout::PaneId,
            shepr_agent::resume::PersistedAgentSession,
        ) {
            use shepr_agent::resume::{AgentSessionRef, PersistedAgentSession};
            let mut app = test_app();
            let workspace = Workspace::test_new("agent");
            let pane_id = workspace.tree().root();
            app.state.test_set_workspaces(vec![workspace]);
            app.state.seed_bookmark_index(Some(0));
            app.insert_idle_test_runtime(pane_id);
            let session = PersistedAgentSession::from_report(
                "shepr:claude",
                AgentSessionRef::id("group-killed").expect("session id"),
            )
            .expect("official session");
            let now = app.clock.now;
            let terminal = app.state.terminal_mut(pane_id);
            terminal
                .ownership_mut()
                .set_detected_agent_process_at(Agent::Claude, now);
            terminal
                .ownership_mut()
                .set_persisted_agent_session(session.clone());
            (app, pane_id, session)
        }

        /// The resume identity the saved session holds for its only pane.
        fn saved_agent_session(app: &App) -> shepr_agent::resume::PersistedAgentSession {
            let lease = shepr_mux::persist::DataDirLease::acquire(app.paths.data_dir())
                .expect("test lease");
            shepr_mux::persist::load(&lease)
                .into_snapshot()
                .expect("saved session")
                .workspaces[0]
                .layout
                .panes()[0]
                .agent_session
                .clone()
                .expect("saved resume identity")
        }

        #[tokio::test]
        async fn a_signal_death_just_after_the_agents_exit_checkpoints_its_identity() {
            let _env = crate::test_support::IsolatedEnv::new();
            let (app, pane_id, session) = app_with_agent_session();
            let mut server = crate::server::headless::tests::test_headless_server();
            server.install_test_app(app);
            release_agent(&mut server.app, pane_id);
            // The release took effect at once: no agent, nothing to resume.
            assert_eq!(
                server
                    .app
                    .state
                    .terminal(pane_id)
                    .expect("terminal")
                    .ownership()
                    .detected_agent(),
                None
            );
            assert_eq!(
                server
                    .app
                    .state
                    .terminal(pane_id)
                    .expect("terminal")
                    .ownership()
                    .current_session_identity_for_persistence(),
                None
            );
            server.persist_for_test();
            server.handle_test_runtime_exit_and_replay(runtime_pane_exit(
                &server.app,
                pane_id,
                shepr_mux::pane::PaneEndReason::Signalled,
                server.app.clock.now + Duration::from_millis(100),
            ));
            server
                .app
                .save_session_before_teardown_async()
                .await
                .expect("final save");
            server.app.retire_session_writer();
            let saved = saved_agent_session(&server.app);
            assert_eq!(saved.session_ref(), session.session_ref());
        }

        #[tokio::test]
        async fn a_signal_shutdown_just_after_the_agents_exit_saves_its_identity() {
            let _env = crate::test_support::IsolatedEnv::new();
            let (mut app, pane_id, session) = app_with_agent_session();
            release_agent(&mut app, pane_id);
            // The release took effect: only the shutdown adoption below brings the
            // identity back.
            assert_eq!(
                app.state
                    .terminal(pane_id)
                    .expect("terminal")
                    .ownership()
                    .current_session_identity_for_persistence(),
                None
            );
            app.persist();
            // The final save after a signal: the pane's death is never processed.
            let quit_at = app.clock.now + Duration::from_millis(100);
            app.state.adopt_checkpoint_candidates_for_shutdown(quit_at);
            assert_eq!(
                app.state
                    .terminal(pane_id)
                    .expect("terminal")
                    .ownership()
                    .current_session_identity_for_persistence()
                    .map(|identity| identity.session_ref().clone()),
                Some(session.session_ref().clone())
            );
            app.save_session_before_teardown_async()
                .await
                .expect("final save");
            app.retire_session_writer();
            let saved = saved_agent_session(&app);
            assert_eq!(saved.session_ref(), session.session_ref());
        }

        #[tokio::test]
        async fn normal_autosave_replaces_a_signaled_exit_checkpoint() {
            let mut server = crate::server::headless::tests::test_headless_server();
            server.install_test_app(test_app());
            server.persist_for_test();
            let workspace = Workspace::test_new("closed");
            let pane_id = workspace.tree().root();
            server.app.state.test_set_workspaces(vec![workspace]);
            server.app.state.seed_bookmark_index(Some(0));
            server.app.insert_idle_test_runtime(pane_id);

            server.handle_test_runtime_exit_and_replay(runtime_pane_exit(
                &server.app,
                pane_id,
                shepr_mux::pane::PaneEndReason::Signalled,
                std::time::Instant::now(),
            ));
            assert!(
                server.app.state.workspaces.is_empty(),
                "the exit was applied"
            );
            // The app still holds the data-dir lease, so the checkpoint is parsed
            // directly rather than through `persist::load`.
            let checkpoint = std::fs::read_to_string(shepr_mux::persist::session_path(
                server.app.paths.data_dir(),
            ))
            .expect("the pane exit writes a checkpoint");
            assert!(shepr_mux::persist::schema::parse_session_file(&checkpoint).is_ok());
            assert!(
                server.app.session_saver.autosave_deadline().is_some(),
                "the pane exit schedules the normal autosave"
            );

            // The loop starts the autosave once its debounce has elapsed.
            let now = server.app.clock.now;
            server.app.session_saver.set_autosave_deadline(Some(now));
            server.app.start_background_session_save();
            assert!(server.app.session_saver.save_in_flight());
            server.app.wait_for_session_save();
            server
                .app
                .save_session_before_teardown_async()
                .await
                .expect("final save");
            server.app.retire_session_writer();

            assert!(
                !shepr_mux::persist::session_path(server.app.paths.data_dir())
                    .try_exists()
                    .expect("test stat")
            );
        }

        #[test]
        fn reader_panic_removes_the_pane_without_a_checkpoint() {
            let mut server = crate::server::headless::tests::test_headless_server();
            server.install_test_app(test_app());
            server.persist_for_test();
            let workspace = Workspace::test_new("broken");
            let pane_id = workspace.tree().root();
            server.app.state.test_set_workspaces(vec![workspace]);
            server.app.state.seed_bookmark_index(Some(0));
            server.app.insert_idle_test_runtime(pane_id);

            server.handle_test_runtime_exit_and_replay(runtime_pane_exit(
                &server.app,
                pane_id,
                shepr_mux::pane::PaneEndReason::ReaderPanicked,
                std::time::Instant::now(),
            ));

            assert!(server.app.state.workspaces.is_empty());
            assert!(
                !shepr_mux::persist::session_path(server.app.paths.data_dir())
                    .try_exists()
                    .expect("test stat")
            );
        }

        #[tokio::test]
        async fn durable_mutation_after_pane_exit_checkpoint_wins_on_shutdown() {
            for another_interrupted_exit in [false, true] {
                let mut server = crate::server::headless::tests::test_headless_server();
                server.install_test_app(test_app());
                server.persist_for_test();
                let workspace = Workspace::test_new("old");
                let pane_id = workspace.tree().root();
                server.app.state.test_set_workspaces(vec![workspace]);
                server.app.state.seed_bookmark_index(Some(0));
                server.app.insert_idle_test_runtime(pane_id);

                server.handle_test_runtime_exit_and_replay(runtime_pane_exit(
                    &server.app,
                    pane_id,
                    shepr_mux::pane::PaneEndReason::Signalled,
                    std::time::Instant::now(),
                ));
                assert!(
                    server.app.state.workspaces.is_empty(),
                    "the first exit was applied"
                );
                server
                    .app
                    .state
                    .test_set_workspaces(vec![Workspace::test_new("newer")]);
                server.app.state.seed_bookmark_index(Some(0));
                server.app.state.mark_session_dirty();
                if another_interrupted_exit {
                    let newer_pane = server.app.state.ws(0).tree().root();
                    server.app.insert_idle_test_runtime(newer_pane);
                    server.handle_test_runtime_exit_and_replay(runtime_pane_exit(
                        &server.app,
                        newer_pane,
                        shepr_mux::pane::PaneEndReason::Signalled,
                        std::time::Instant::now(),
                    ));
                    assert!(
                        server.app.state.workspaces.is_empty(),
                        "the second exit was applied"
                    );
                }
                server
                    .app
                    .save_session_before_teardown_async()
                    .await
                    .expect("final save");
                server.app.retire_session_writer();

                let lease = shepr_mux::persist::DataDirLease::acquire(server.app.paths.data_dir())
                    .expect("test lease");
                let snapshot = shepr_mux::persist::load(&lease)
                    .into_snapshot()
                    .expect("newer session should be saved");
                assert_eq!(snapshot.workspaces.len(), 1);
                assert_eq!(snapshot.workspaces[0].name.as_str(), "newer");
            }
        }
    }
}
