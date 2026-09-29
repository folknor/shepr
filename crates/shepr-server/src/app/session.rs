use std::time::{Duration, Instant, SystemTime};

use super::App;
use crate::limits::{
    CHECKPOINT_MAX_FAILURES, HOST_SHUTDOWN_CHECKPOINT_RETRY_MAX_DELAY, SESSION_SAVE_CHECK_INTERVAL,
    SESSION_SAVE_DEBOUNCE, SESSION_SAVE_RETRY_MAX, SESSION_SAVE_RETRY_MIN,
};
#[derive(Clone, Copy)]
enum SessionSavePurpose {
    Autosave,
    Checkpoint {
        host_shutdown_generation: Option<u64>,
        pane_exit_generation: Option<u64>,
    },
}

pub(crate) struct SessionSaver {
    pub(crate) session_save_deadline: Option<Instant>,
    session_save_check_deadline: Option<Instant>,
    /// Consecutive failed saves, for the retry backoff.
    failed_saves: u32,
    pub(crate) session_save_thread: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    session_save_purpose: Option<SessionSavePurpose>,
    session_writer: SessionWriterHandle,
    pub(crate) pane_exit_checkpoint_pending: bool,
    pane_exit_checkpoint_requested: bool,
    pane_exit_checkpoint_generation: u64,
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

/// Serializes operations on the session writer and owns their poison policy. A
/// save retry checks the history digest and file stamp before trusting its
/// cache. Retirement uses the same recovered guard to release the data
/// directory lease.
#[derive(Clone)]
struct SessionWriterHandle(std::sync::Arc<std::sync::Mutex<shepr_mux::persist::SessionWriter>>);

impl SessionWriterHandle {
    fn new(writer: std::sync::Arc<std::sync::Mutex<shepr_mux::persist::SessionWriter>>) -> Self {
        Self(writer)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, shepr_mux::persist::SessionWriter> {
        shepr_vt::lock_auxiliary(&self.0)
    }
}

impl SessionSaver {
    pub(crate) fn new(
        writer: std::sync::Arc<std::sync::Mutex<shepr_mux::persist::SessionWriter>>,
    ) -> Self {
        Self {
            session_save_deadline: None,
            session_save_check_deadline: None,
            failed_saves: 0,
            session_save_thread: None,
            session_save_purpose: None,
            session_writer: SessionWriterHandle::new(writer),
            pane_exit_checkpoint_pending: false,
            pane_exit_checkpoint_requested: false,
            pane_exit_checkpoint_generation: 0,
            pane_exit_checkpoint_failures: 0,
            pane_exit_checkpoint_ready: false,
            critical_save_retry_deadline: None,
            host_shutdown_checkpoint_generation: 0,
            host_shutdown_checkpoint_requested: false,
            host_shutdown_checkpoint_failures: 0,
            host_shutdown_checkpoint_result: None,
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        [
            self.session_save_deadline,
            self.session_save_check_deadline,
            self.critical_save_retry_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(crate) fn is_due(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| now >= deadline)
    }

    pub(crate) fn clear_deadline(&mut self) {
        self.session_save_deadline = None;
        self.session_save_check_deadline = None;
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
        self.pane_exit_checkpoint_pending = false;
        self.session_save_deadline = Some(now + SESSION_SAVE_DEBOUNCE);
    }

    fn retry(&mut self, now: Instant) {
        self.retry_after(now, SESSION_SAVE_RETRY_MIN);
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

enum SessionSaveJob {
    Clear,
    Save {
        snapshot: shepr_mux::persist::SessionSnapshot,
        history: Option<shepr_mux::persist::PendingHistory>,
    },
}

impl App {
    pub(super) fn schedule_session_save(&mut self) {
        if self.policy.persists_session() {
            self.session_saver.schedule(self.clock.now);
        }
    }

    pub(crate) fn sync_session_save_schedule(&mut self) {
        if self.state.session_dirty {
            self.state.session_dirty = false;
            self.schedule_session_save();
        }
    }

    fn reap_finished_session_save(&mut self, now: Instant) {
        if self
            .session_saver
            .session_save_thread
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
            && let Some(thread) = self.session_saver.session_save_thread.take()
        {
            self.session_saver.session_save_check_deadline = None;
            let purpose = self
                .session_saver
                .session_save_purpose
                .take()
                .unwrap_or(SessionSavePurpose::Autosave);
            let result = match thread.join() {
                Ok(result) => result,
                Err(_) => Err(std::io::Error::other("session save thread panicked")),
            };
            self.finish_session_save(purpose, result, now);
        }
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

    fn finish_session_save(
        &mut self,
        purpose: SessionSavePurpose,
        result: std::io::Result<()>,
        now: Instant,
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
                    if pane_exit_generation
                        == Some(self.session_saver.pane_exit_checkpoint_generation)
                        && self.session_saver.pane_exit_checkpoint_requested
                    {
                        self.complete_pane_exit_checkpoint();
                    } else if self.session_saver.pane_exit_checkpoint_requested {
                        self.session_saver.session_save_deadline = None;
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

    fn complete_pane_exit_checkpoint(&mut self) {
        self.session_saver.pane_exit_checkpoint_requested = false;
        self.session_saver.pane_exit_checkpoint_pending = true;
        self.session_saver.pane_exit_checkpoint_ready = true;
    }

    /// Runs on the event loop, so it takes only what must be read here: the
    /// structural snapshot and each pane's history source. Turning history
    /// into its saved form is left to `run_session_save_job`.
    fn capture_session_save_job(&self) -> SessionSaveJob {
        if self.state.workspaces.is_empty() {
            SessionSaveJob::Clear
        } else {
            let snapshot = shepr_mux::persist::capture(
                &self.state.workspaces,
                &self.state.terminals,
                &self.terminal_runtimes,
                self.paths
                    .current_dir()
                    .unwrap_or_else(|| std::path::Path::new("/")),
                self.state.active_index(),
                self.state.selected_index().unwrap_or(0),
                self.state.host_terminal_theme,
            );
            let history = self.persist_pane_history.then(|| {
                shepr_mux::persist::capture_pending_history(
                    &self.state.workspaces,
                    &self.terminal_runtimes,
                    &self.pane_history_carry,
                )
            });
            SessionSaveJob::Save { snapshot, history }
        }
    }

    pub(crate) fn start_background_session_save(&mut self) {
        if !self.policy.persists_session() && !self.session_saver.pane_exit_checkpoint_requested {
            self.session_saver.session_save_deadline = None;
            self.session_saver.session_save_check_deadline = None;
            self.session_saver.critical_save_retry_deadline = None;
            return;
        }

        let now = self.clock.now;
        self.reap_finished_session_save(now);
        if self
            .session_saver
            .host_shutdown_checkpoint_result
            .is_some_and(|(_, saved)| !saved)
            && !self.session_saver.pane_exit_checkpoint_requested
        {
            return;
        }
        if self.session_saver.session_save_thread.is_some() {
            if self.session_saver.save_is_due(now) {
                self.session_saver.retry(now);
            }
            self.session_saver.session_save_check_deadline =
                Some(now + SESSION_SAVE_CHECK_INTERVAL);
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
            self.session_saver.session_save_check_deadline = None;
            self.session_saver.critical_save_retry_deadline = None;
            self.state.session_dirty = false;
            self.spawn_session_save(
                self.capture_session_save_job(),
                SessionSavePurpose::Checkpoint {
                    host_shutdown_generation,
                    pane_exit_generation,
                },
                now,
            );
        } else if self.session_saver.save_is_due(now) {
            self.session_saver.session_save_deadline = None;
            self.session_saver.session_save_check_deadline = None;
            self.spawn_session_save(
                self.capture_session_save_job(),
                SessionSavePurpose::Autosave,
                now,
            );
        }
    }

    fn spawn_session_save(
        &mut self,
        job: SessionSaveJob,
        purpose: SessionSavePurpose,
        now: Instant,
    ) {
        let writer = self.session_saver.session_writer.clone();
        let saved_at = self.clock.wall_now;
        match std::thread::Builder::new()
            .name("shepr-session-save".into())
            .spawn(move || run_session_save_job(job, &writer, saved_at))
        {
            Ok(thread) => {
                self.session_saver.session_save_thread = Some(thread);
                self.session_saver.session_save_purpose = Some(purpose);
                self.session_saver.session_save_check_deadline =
                    Some(now + SESSION_SAVE_CHECK_INTERVAL);
            }
            Err(err) => self.finish_session_save(
                purpose,
                Err(std::io::Error::new(
                    err.kind(),
                    format!("failed to spawn session save thread: {err}"),
                )),
                now,
            ),
        }
    }

    /// Whether an exited pane may be removed now: nothing is persisted, the
    /// pre-exit layout is already on disk, or checkpoints have been abandoned
    /// after repeated failures.
    pub(crate) fn pane_exit_checkpoint_settled(&self) -> bool {
        let saver = &self.session_saver;
        (!self.policy.persists_session() && !saver.pane_exit_checkpoint_requested)
            || (saver.pane_exit_checkpoint_pending && !self.state.session_dirty)
            || saver.pane_exit_checkpoint_failures >= CHECKPOINT_MAX_FAILURES
    }

    /// Returns true when the exited pane may be removed now. Otherwise starts a
    /// background checkpoint of the pre-exit layout; the headless caller holds
    /// the exit until the save worker reports success or retries run out.
    pub(crate) fn checkpoint_session_before_pane_exit(&mut self) -> bool {
        if self.pane_exit_checkpoint_settled() {
            return true;
        }
        self.session_saver.pane_exit_checkpoint_requested = true;
        self.session_saver.pane_exit_checkpoint_generation = self
            .session_saver
            .pane_exit_checkpoint_generation
            .saturating_add(1);
        self.start_background_session_save();
        false
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
        if self.session_saver.pane_exit_checkpoint_pending {
            self.state.session_dirty = false;
            self.session_saver.session_save_deadline = Some(self.clock.now + SESSION_SAVE_DEBOUNCE);
        }
    }

    pub(crate) async fn save_session_before_teardown_async(&mut self) {
        let preserve_checkpoint =
            self.session_saver.pane_exit_checkpoint_pending && !self.state.session_dirty;

        if let Some(thread) = self.session_saver.session_save_thread.take() {
            self.session_saver.session_save_check_deadline = None;
            let purpose = self
                .session_saver
                .session_save_purpose
                .take()
                .unwrap_or(SessionSavePurpose::Autosave);
            let result = match tokio::task::spawn_blocking(move || thread.join()).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(std::io::Error::other("session save thread panicked")),
                Err(err) => Err(std::io::Error::other(format!(
                    "failed to join session save thread: {err}"
                ))),
            };
            self.finish_session_save(purpose, result, self.clock.now);
        }

        if preserve_checkpoint {
            self.session_saver.clear_deadline();
            return;
        }

        if !self.policy.persists_session() {
            self.session_saver.clear_deadline();
            return;
        }

        let job = self.capture_session_save_job();
        let writer = self.session_saver.session_writer.clone();
        let saved_at = self.clock.wall_now;
        let result =
            match tokio::task::spawn_blocking(move || run_session_save_job(job, &writer, saved_at))
                .await
            {
                Ok(result) => result,
                Err(err) => Err(std::io::Error::other(format!(
                    "session save worker failed: {err}"
                ))),
            };
        self.session_saver.pane_exit_checkpoint_pending = false;
        let saved = self.record_session_save_result(result, self.clock.now);
        if saved {
            self.session_saver.clear_deadline();
        }
    }

    pub(crate) fn retire_session_writer(&mut self) {
        if let Some(thread) = self.session_saver.session_save_thread.take() {
            // The last save still in flight at shutdown: its failure is
            // logged like any other save's. The retry it schedules is moot,
            // the deadline is cleared below and the writer retired.
            let result = thread
                .join()
                .unwrap_or_else(|_| Err(std::io::Error::other("session save thread panicked")));
            self.record_session_save_result(result, self.clock.now);
        }
        self.session_saver.clear_deadline();
        self.session_saver.session_writer.lock().retire();
    }
}

fn run_session_save_job(
    job: SessionSaveJob,
    writer: &SessionWriterHandle,
    now: SystemTime,
) -> std::io::Result<()> {
    // Formatting pane history is the expensive part of a save; it happens
    // here, before the writer is locked.
    let job = match job {
        SessionSaveJob::Clear => None,
        SessionSaveJob::Save { snapshot, history } => {
            let history = history.map(|history| history.resolve(&snapshot));
            Some((snapshot, history))
        }
    };
    let mut writer = writer.lock();
    match job {
        None => writer.clear(now),
        Some((snapshot, history)) => writer.save(&snapshot, history.as_ref(), now),
    }
}

#[cfg(test)]
use shepr_mux::events::AppEvent;

#[cfg(test)]
impl App {
    pub(crate) fn save_session_now(&mut self) -> bool {
        if let Some(thread) = self.session_saver.session_save_thread.take() {
            self.session_saver.session_save_check_deadline = None;
            let purpose = self
                .session_saver
                .session_save_purpose
                .take()
                .unwrap_or(SessionSavePurpose::Autosave);
            let result = match thread.join() {
                Ok(result) => result,
                Err(_) => Err(std::io::Error::other("session save thread panicked")),
            };
            self.finish_session_save(purpose, result, self.clock.now);
        }

        if !self.policy.persists_session() {
            self.session_saver.clear_deadline();
            return true;
        }

        let result = run_session_save_job(
            self.capture_session_save_job(),
            &self.session_saver.session_writer,
            self.clock.wall_now,
        );
        self.session_saver.pane_exit_checkpoint_pending = false;
        let saved = self.record_session_save_result(result, self.clock.now);
        if saved {
            self.session_saver.clear_deadline();
        }
        saved
    }

    /// Delivers `ev` the way the headless loop does: a pane exit that needs a
    /// checkpoint waits for the background save before the app removes it.
    pub(crate) fn handle_internal_event_after_checkpoint(&mut self, ev: AppEvent) {
        if let AppEvent::PaneDied {
            pane_id,
            exit_reason,
        } = &ev
            && exit_reason.requires_session_checkpoint()
            && self.state.prepare_pane_removal_by_id(*pane_id).is_some()
            && !self.checkpoint_session_before_pane_exit()
        {
            for _ in 0..4 {
                if let Some(thread) = self.session_saver.session_save_thread.take() {
                    let purpose = self
                        .session_saver
                        .session_save_purpose
                        .take()
                        .unwrap_or(SessionSavePurpose::Autosave);
                    let result = thread
                        .join()
                        .unwrap_or_else(|_| Err(std::io::Error::other("save thread panicked")));
                    self.finish_session_save(purpose, result, self.clock.now);
                }
                if self.take_pane_exit_checkpoint_ready() {
                    break;
                }
                self.session_saver.critical_save_retry_deadline = None;
                self.start_background_session_save();
            }
        }
        self.handle_internal_event(ev);
    }

    /// Save the live pane histories while runtimes still exist, keeping the
    /// directory claim until their processes have finished tearing down.
    pub(crate) fn save_session_before_teardown(&mut self) {
        if self.session_saver.pane_exit_checkpoint_pending && !self.state.session_dirty {
            self.session_saver.clear_deadline();
        } else {
            self.save_session_now();
        }
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
            shepr_api::EventHub::default(),
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
            app.finish_session_save(purpose, disk_full(), now);
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

        app.finish_session_save(purpose, disk_full(), now);
        assert!(
            app.take_pane_exit_checkpoint_ready(),
            "the last failure releases the held exit for removal"
        );
        assert!(!app.pane_exit_checkpoint_requested());
        assert!(app.session_saver.critical_save_retry_deadline.is_none());
        assert!(
            app.checkpoint_session_before_pane_exit(),
            "later exits are removed without waiting on a failing disk"
        );

        app.finish_session_save(SessionSavePurpose::Autosave, Ok(()), now);
        assert!(
            !app.pane_exit_checkpoint_settled(),
            "a save that succeeds again restores pre-exit checkpoints"
        );
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

    /// A restore that drops a saved tab leaves that tab only in the session
    /// file, so the first save copies the file to `session-backups` before
    /// replacing it. The copy is made once, not on every save.
    #[test]
    fn a_restore_that_drops_a_tab_backs_up_the_saved_session_before_the_first_save() {
        use crate::test_support::{AppPathsFixture as _, ValidatedConfigFixture as _};
        use shepr_mux::persist::snapshot::{
            DirectionSnapshot, LayoutSnapshot, PaneSnapshot, SessionSnapshot, TabSnapshot,
            WorkspaceSnapshot,
        };

        let scratch = crate::test_support::ScratchDir::new("dropped-tab-backup");
        let paths = shepr_config::AppPaths::test_at(&scratch);
        let config = shepr_config::ValidatedConfig::test_from_config_with_paths(
            shepr_config::Config::default(),
            None,
            paths.clone(),
        );
        let data_dir = shepr_api::session::data_dir(&paths);
        let lease =
            shepr_mux::persist::DataDirLease::acquire(&data_dir).expect("test session lease");

        // A working directory that does not exist restores each pane without
        // starting a shell.
        let pane = || PaneSnapshot {
            cwd: scratch.join("missing-cwd"),
            label: None,
            agent_session: None,
            launch_argv: None,
        };
        let tab = |name: &str, layout: LayoutSnapshot, ids: &[u32]| TabSnapshot {
            custom_name: Some(name.into()),
            layout,
            panes: ids.iter().map(|id| (*id, pane())).collect(),
            zoomed: false,
            focused: None,
            root_pane: None,
        };
        let snapshot = SessionSnapshot {
            version: shepr_mux::persist::snapshot::SNAPSHOT_VERSION,
            host_theme: Default::default(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("w1".into()),
                custom_name: Some("mixed".into()),
                identity_cwd: scratch.path().to_path_buf(),
                public_pane_numbers: std::collections::HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![
                    tab("healthy", LayoutSnapshot::Pane(1), &[1]),
                    tab(
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
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
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
            shepr_api::EventHub::default(),
            super::super::tests::test_clock(),
        );
        let tab_names = |workspaces: Vec<Vec<Option<String>>>| workspaces.concat();
        assert_eq!(
            tab_names(
                app.state
                    .workspaces
                    .iter()
                    .map(|workspace| {
                        workspace
                            .tabs()
                            .iter()
                            .map(|tab| tab.custom_name().map(str::to_owned))
                            .collect()
                    })
                    .collect()
            ),
            vec![Some("healthy".to_owned())],
            "the saved session loaded and only the invalid tab was dropped"
        );

        assert!(app.save_session_now(), "first save");
        let backups = data_dir.join("session-backups");
        assert_eq!(directory_files(&backups), vec![original.clone()]);
        let saved = shepr_mux::persist::snapshot::parse_snapshot(
            &std::fs::read_to_string(&session_file).expect("read the new session"),
        )
        .expect("parse the new session");
        assert_eq!(
            tab_names(
                saved
                    .workspaces
                    .iter()
                    .map(|workspace| workspace
                        .tabs
                        .iter()
                        .map(|tab| tab.custom_name.clone())
                        .collect())
                    .collect()
            ),
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
}
