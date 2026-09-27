use std::time::{Duration, Instant};

use super::{App, SESSION_SAVE_DEBOUNCE};

const SESSION_SAVE_RETRY_MIN: Duration = Duration::from_millis(250);
const SESSION_SAVE_RETRY_MAX: Duration = Duration::from_secs(30);

pub(crate) struct SessionSaver {
    pub(crate) session_save_deadline: Option<Instant>,
    session_save_check_deadline: Option<Instant>,
    /// Consecutive failed saves, for the retry backoff.
    failed_saves: u32,
    pub(crate) session_save_thread: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    pub(crate) session_writer: std::sync::Arc<std::sync::Mutex<shepr_mux::persist::SessionWriter>>,
    pub(crate) pane_exit_checkpoint_pending: bool,
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
            session_writer: writer,
            pane_exit_checkpoint_pending: false,
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        match (self.session_save_deadline, self.session_save_check_deadline) {
            (Some(save), Some(check)) => Some(save.min(check)),
            (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
            (None, None) => None,
        }
    }

    pub(crate) fn is_due(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| now >= deadline)
    }

    pub(crate) fn clear_deadline(&mut self) {
        self.session_save_deadline = None;
        self.session_save_check_deadline = None;
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
            self.session_saver.schedule(Instant::now());
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
            let result = match thread.join() {
                Ok(result) => result,
                Err(_) => Err(std::io::Error::other("session save thread panicked")),
            };
            self.record_session_save_result(result, now);
        }
    }

    pub(super) fn record_session_save_result(
        &mut self,
        result: std::io::Result<()>,
        now: Instant,
    ) -> bool {
        match result {
            Err(err) => {
                let delay = self.session_saver.retry_after_failure(now);
                tracing::warn!(
                    err = %err,
                    failures = self.session_saver.failed_saves,
                    retry_ms = delay.as_millis(),
                    "session save failed; scheduling a retry"
                );
                false
            }
            Ok(()) => {
                if self.session_saver.failed_saves > 0 {
                    tracing::info!(
                        failures = self.session_saver.failed_saves,
                        "session save recovered after failures"
                    );
                    self.session_saver.failed_saves = 0;
                }
                true
            }
        }
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
        if !self.policy.persists_session() {
            self.session_saver.session_save_deadline = None;
            self.session_saver.session_save_check_deadline = None;
            return;
        }

        let now = Instant::now();
        self.reap_finished_session_save(now);
        if self.session_saver.session_save_thread.is_some() {
            if self.session_saver.save_is_due(now) {
                self.session_saver.retry(now);
            }
            self.session_saver.session_save_check_deadline = Some(now + Duration::from_millis(250));
            return;
        }
        if !self.session_saver.save_is_due(now) {
            return;
        }

        let job = self.capture_session_save_job();
        self.session_saver.pane_exit_checkpoint_pending = false;
        self.session_saver.session_save_deadline = None;
        self.session_saver.session_save_check_deadline = None;
        let writer = std::sync::Arc::clone(&self.session_saver.session_writer);
        // Keep the filesystem work off the Tokio loop. If the worker cannot
        // start, the saver captures a fresh snapshot on its scheduled retry.
        match std::thread::Builder::new()
            .name("shepr-session-save".into())
            .spawn(move || run_session_save_job(job, &writer))
        {
            Ok(thread) => {
                self.session_saver.session_save_thread = Some(thread);
                self.session_saver.session_save_check_deadline =
                    Some(now + Duration::from_millis(250));
            }
            Err(err) => {
                self.record_session_save_result(
                    Err(std::io::Error::new(
                        err.kind(),
                        format!("failed to spawn session save thread: {err}"),
                    )),
                    now,
                );
            }
        }
    }

    pub(crate) fn save_session_now(&mut self) -> bool {
        if let Some(thread) = self.session_saver.session_save_thread.take() {
            self.session_saver.session_save_check_deadline = None;
            let result = match thread.join() {
                Ok(result) => result,
                Err(_) => Err(std::io::Error::other("session save thread panicked")),
            };
            self.record_session_save_result(result, Instant::now());
        }

        if !self.policy.persists_session() {
            self.session_saver.clear_deadline();
            return true;
        }

        let result = run_session_save_job(
            self.capture_session_save_job(),
            &self.session_saver.session_writer,
        );
        self.session_saver.pane_exit_checkpoint_pending = false;
        let saved = self.record_session_save_result(result, Instant::now());
        if saved {
            self.session_saver.clear_deadline();
        }
        saved
    }

    pub(crate) fn checkpoint_session_before_pane_exit(&mut self) {
        if !self.policy.persists_session()
            || (self.session_saver.pane_exit_checkpoint_pending && !self.state.session_dirty)
        {
            return;
        }
        if !self.save_session_now() {
            return;
        }
        self.session_saver.pane_exit_checkpoint_pending = true;
        self.state.session_dirty = false;
    }

    pub(crate) fn finish_checkpointed_pane_exit(&mut self) {
        if self.session_saver.pane_exit_checkpoint_pending {
            self.state.session_dirty = false;
            self.session_saver.session_save_deadline = Some(Instant::now() + SESSION_SAVE_DEBOUNCE);
        }
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

    pub(crate) fn retire_session_writer(&mut self) {
        if let Some(thread) = self.session_saver.session_save_thread.take() {
            let _ = thread.join();
        }
        self.session_saver.clear_deadline();
        // Retiring only drops the lock and marks the writer done; a panic in
        // an earlier save cannot leave anything here half-updated.
        self.session_saver
            .session_writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retire();
    }
}

fn run_session_save_job(
    job: SessionSaveJob,
    writer: &std::sync::Mutex<shepr_mux::persist::SessionWriter>,
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
    let mut writer = match writer.lock() {
        Ok(writer) => writer,
        Err(err) => {
            tracing::warn!(err = %err, "session writer is poisoned; refusing to modify session");
            return Err(std::io::Error::other("session writer mutex is poisoned"));
        }
    };
    match job {
        None => writer.clear(),
        Some((snapshot, history)) => writer.save(&snapshot, history.as_ref()),
    }
}
