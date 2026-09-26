use std::time::{Duration, Instant};

use super::{App, SESSION_SAVE_DEBOUNCE};

pub(crate) struct SessionSaver {
    pub(crate) session_save_deadline: Option<Instant>,
    pub(crate) session_save_thread: Option<std::thread::JoinHandle<()>>,
    pub(crate) session_writer: std::sync::Arc<std::sync::Mutex<crate::persist::SessionWriter>>,
    pub(crate) pane_exit_checkpoint_pending: bool,
}

impl SessionSaver {
    pub(crate) fn new(
        writer: std::sync::Arc<std::sync::Mutex<crate::persist::SessionWriter>>,
    ) -> Self {
        Self {
            session_save_deadline: None,
            session_save_thread: None,
            session_writer: writer,
            pane_exit_checkpoint_pending: false,
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.session_save_deadline
    }

    pub(crate) fn is_due(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| now >= deadline)
    }

    pub(crate) fn clear_deadline(&mut self) {
        self.session_save_deadline = None;
    }

    fn schedule(&mut self, now: Instant) {
        self.pane_exit_checkpoint_pending = false;
        self.session_save_deadline = Some(now + SESSION_SAVE_DEBOUNCE);
    }

    fn retry(&mut self, now: Instant) {
        self.session_save_deadline = Some(now + Duration::from_millis(250));
    }
}

enum SessionSaveJob {
    Clear,
    Save {
        snapshot: crate::persist::SessionSnapshot,
        history: Option<crate::persist::PendingHistory>,
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

    fn reap_finished_session_save(&mut self) {
        if self
            .session_saver
            .session_save_thread
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
            && let Some(thread) = self.session_saver.session_save_thread.take()
        {
            let _ = thread.join();
        }
    }

    /// Runs on the event loop, so it takes only what must be read here: the
    /// structural snapshot and each pane's history source. Turning history
    /// into its saved form is left to `run_session_save_job`.
    fn capture_session_save_job(&self) -> SessionSaveJob {
        if self.state.workspaces.is_empty() {
            SessionSaveJob::Clear
        } else {
            let snapshot = crate::persist::capture(
                &self.state.workspaces,
                &self.state.terminals,
                &self.terminal_runtimes,
                self.paths
                    .current_dir()
                    .unwrap_or_else(|| std::path::Path::new("/")),
                self.state.active,
                self.state.selected,
                self.state.host_terminal_theme,
            );
            let history = self.persist_pane_history.then(|| {
                crate::persist::capture_pending_history(
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
            return;
        }

        self.reap_finished_session_save();
        if self.session_saver.session_save_thread.is_some() {
            self.session_saver.retry(Instant::now());
            return;
        }

        let job = self.capture_session_save_job();
        self.session_saver.pane_exit_checkpoint_pending = false;
        self.session_saver.session_save_deadline = None;
        let writer = std::sync::Arc::clone(&self.session_saver.session_writer);
        // The job goes to the thread over a channel rather than inside the
        // closure, so a failed spawn hands it back to be saved inline instead
        // of capturing everything a second time.
        let (job_tx, job_rx) = std::sync::mpsc::sync_channel::<SessionSaveJob>(1);
        match std::thread::Builder::new()
            .name("shepr-session-save".into())
            .spawn(move || {
                if let Ok(job) = job_rx.recv() {
                    run_session_save_job(job, &writer);
                }
            }) {
            Ok(thread) => {
                if let Err(std::sync::mpsc::SendError(job)) = job_tx.send(job) {
                    // Only possible if the thread is already gone.
                    run_session_save_job(job, &self.session_saver.session_writer);
                }
                self.session_saver.session_save_thread = Some(thread);
            }
            Err(err) => {
                tracing::warn!(err = %err, "failed to spawn session save thread; saving inline");
                run_session_save_job(job, &self.session_saver.session_writer);
            }
        }
    }

    pub(crate) fn save_session_now(&mut self) {
        if let Some(thread) = self.session_saver.session_save_thread.take() {
            let _ = thread.join();
        }

        if !self.policy.persists_session() {
            self.session_saver.session_save_deadline = None;
            return;
        }

        run_session_save_job(
            self.capture_session_save_job(),
            &self.session_saver.session_writer,
        );
        self.session_saver.pane_exit_checkpoint_pending = false;
        self.session_saver.session_save_deadline = None;
    }

    pub(crate) fn checkpoint_session_before_pane_exit(&mut self) {
        if !self.policy.persists_session()
            || (self.session_saver.pane_exit_checkpoint_pending && !self.state.session_dirty)
        {
            return;
        }
        self.save_session_now();
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
            self.session_saver.session_save_deadline = None;
        } else {
            self.save_session_now();
        }
    }

    pub(crate) fn retire_session_writer(&mut self) {
        if let Some(thread) = self.session_saver.session_save_thread.take() {
            let _ = thread.join();
        }
        self.session_saver.session_save_deadline = None;
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
    writer: &std::sync::Mutex<crate::persist::SessionWriter>,
) {
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
            return;
        }
    };
    match job {
        None => writer.clear(),
        Some((snapshot, history)) => writer.save(&snapshot, history.as_ref()),
    }
}
