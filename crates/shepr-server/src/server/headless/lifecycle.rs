use super::HeadlessServer;
use crate::app;
use crate::limits::{SHUTDOWN_FLUSH_TIMEOUT, STOP_ANSWER_WAIT};
use crate::server::outbox::ReleaseMode;
use shepr_protocol::ServerMessage;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::debug;

mod host_shutdown;
use host_shutdown::HostShutdownMonitor;

/// The final save result a stop request is answered with when the server
/// exits without having run its final save.
pub(super) const UNFINISHED_FINAL_SAVE_MESSAGE: &str =
    "the server exited before it ran its final session save";

/// The server lifecycle states that can affect saves or request handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShutdownPhase {
    Running,
    HostShutdownWarning,
    Frozen {
        /// The warning answered by the checkpoint. Refreshing it requires
        /// another checkpoint even if cancellation was not observed.
        generation: Option<WarningGeneration>,
    },
    Stopping,
}

/// Lifecycle operation names kept typed until an error is presented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShutdownStep {
    CompleteShutdown,
}

impl std::fmt::Display for ShutdownStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CompleteShutdown => f.write_str("completing shutdown"),
        }
    }
}

/// A lifecycle step was asked for from a phase it does not start from. The
/// step is refused and the phase left as it was, so an illegal transition
/// never lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnexpectedPhase {
    pub(super) step: ShutdownStep,
    pub(super) expected: ShutdownPhase,
    pub(super) actual: ShutdownPhase,
}

impl std::fmt::Display for UnexpectedPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} needs the {:?} lifecycle phase, but the server is in {:?}",
            self.step, self.expected, self.actual
        )
    }
}

impl std::error::Error for UnexpectedPhase {}

/// Identity of one logind shutdown warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WarningGeneration(u64);

impl WarningGeneration {
    pub(super) fn as_u64(self) -> u64 {
        self.0
    }
}

/// Owns the server's lifecycle phase and the asynchronous request latches that
/// feed it. The latches are written by signal, API and logind threads; the
/// event loop is the sole owner of phase transitions and session policy.
pub(super) struct ShutdownLifecycle {
    phase: ShutdownPhase,
    monitor: Option<HostShutdownMonitor>,
    /// The warning the current host checkpoint answers: set when a warning
    /// starts or refreshes the checkpoint, cleared when the shutdown is called
    /// off. A warning whose generation no longer matches gets a fresh
    /// checkpoint before the delay lock is released.
    checkpoint_generation: Option<WarningGeneration>,
    stop_signal: Arc<shepr_api::ServerStopSignal>,
    host_shutdown_request: Arc<AtomicBool>,
    /// When the first termination signal arrived, set by the signal handler.
    /// Later signals leave it as it is.
    signal_quit_request: Arc<std::sync::OnceLock<std::time::Instant>>,
}

impl ShutdownLifecycle {
    pub(super) fn new(stop_signal: Arc<shepr_api::ServerStopSignal>) -> Self {
        Self {
            phase: ShutdownPhase::Running,
            monitor: None,
            checkpoint_generation: None,
            stop_signal,
            host_shutdown_request: Arc::new(AtomicBool::new(false)),
            signal_quit_request: Arc::default(),
        }
    }

    pub(super) fn phase(&self) -> ShutdownPhase {
        self.phase
    }

    /// Whether a stop source has requested termination, or termination has
    /// already begun.
    pub(super) fn stop_requested(&self) -> bool {
        self.phase == ShutdownPhase::Stopping || self.stop_signal.is_requested()
    }

    pub(super) fn stop_signal(&self) -> &Arc<shepr_api::ServerStopSignal> {
        &self.stop_signal
    }

    pub(super) fn host_shutdown_request_flag(&self) -> &Arc<AtomicBool> {
        &self.host_shutdown_request
    }

    pub(super) fn signal_quit_request_flag(&self) -> &Arc<std::sync::OnceLock<std::time::Instant>> {
        &self.signal_quit_request
    }

    pub(super) fn signal_quit_requested(&self) -> bool {
        self.signal_quit_request.get().is_some()
    }

    /// When the first termination signal arrived, if one has.
    pub(super) fn signal_quit_at(&self) -> Option<std::time::Instant> {
        self.signal_quit_request.get().copied()
    }

    pub(super) fn host_shutdown_requested(&self) -> bool {
        self.host_shutdown_request.load(Ordering::Acquire)
    }

    pub(super) fn begin_host_shutdown_warning(&mut self) -> bool {
        if self.phase != ShutdownPhase::Running {
            return false;
        }
        self.phase = ShutdownPhase::HostShutdownWarning;
        true
    }

    /// Refuses `step` unless the server is in `expected`.
    pub(super) fn require_phase(
        &self,
        step: ShutdownStep,
        expected: ShutdownPhase,
    ) -> Result<(), UnexpectedPhase> {
        if self.phase == expected {
            Ok(())
        } else {
            Err(UnexpectedPhase {
                step,
                expected,
                actual: self.phase,
            })
        }
    }

    /// Cancels either a warning not yet checkpointed or a completed freeze.
    /// Returns whether the saver was frozen and the caller must thaw it.
    pub(super) fn cancel_host_shutdown(&mut self) -> bool {
        match self.phase {
            ShutdownPhase::HostShutdownWarning => {
                self.phase = ShutdownPhase::Running;
                false
            }
            ShutdownPhase::Frozen { .. } => {
                self.phase = ShutdownPhase::Running;
                true
            }
            ShutdownPhase::Running | ShutdownPhase::Stopping => false,
        }
    }

    /// Starts a fresh checkpoint after logind issued another warning before
    /// the loop observed cancellation of the previous one.
    pub(super) fn restart_host_shutdown_warning(&mut self) -> bool {
        if !matches!(self.phase, ShutdownPhase::Frozen { .. }) {
            return false;
        }
        self.phase = ShutdownPhase::HostShutdownWarning;
        true
    }

    pub(super) fn frozen_warning_generation(&self) -> Option<WarningGeneration> {
        match self.phase {
            ShutdownPhase::Frozen { generation } => generation,
            _ => None,
        }
    }

    /// Enters the terminal phase. `fallback` is the reason recorded when no
    /// stop request named one first; the first recorded reason always wins.
    pub(super) fn begin_stopping(&mut self, fallback: shepr_api::StopReason) -> bool {
        if self.phase == ShutdownPhase::Stopping {
            return false;
        }
        self.phase = ShutdownPhase::Stopping;
        self.stop_signal.request(fallback);
        true
    }

    /// The canonical rejection of a JSON API request selected after the
    /// server entered its terminal stopping phase. An endpoint command gets
    /// `EndpointError::ShuttingDown` instead.
    pub(super) fn shutdown_error(&self) -> shepr_api::error::ApiError {
        // This is a fixed refusal response, with no phase-dependent data.
        // Constructing it must not panic while the server drains requests.
        // A stopping proof token would only police callers of this pure value
        // constructor; transition validation belongs to the lifecycle steps.
        shepr_api::error::ApiError::new(
            shepr_api::error::ApiErrorCode::ServerUnavailable,
            shepr_protocol::ShutdownReason::Stopping.to_string(),
        )
    }
}

impl ShutdownLifecycle {
    pub(super) fn start_host_shutdown_monitor(&mut self, wake: &Arc<tokio::sync::Notify>) {
        if self.monitor.is_some() {
            return;
        }
        let wake_loop = Arc::clone(wake);
        self.monitor = Some(HostShutdownMonitor::start(
            Arc::clone(self.host_shutdown_request_flag()),
            move || {
                // The monitor updates the request flag first. The loop reads
                // that state before its next internal-event batch.
                wake_loop.notify_one();
            },
        ));
    }

    /// Applies warning and cancellation notifications from logind to the
    /// lifecycle state machine. A warning checkpoints before freezing saves;
    /// cancellation thaws and marks the live session dirty again.
    pub(super) fn sync_host_shutdown_freeze(&mut self, app: &mut app::App) {
        if self.phase() == ShutdownPhase::Stopping {
            return;
        }

        let generation = self
            .monitor
            .as_ref()
            .and_then(HostShutdownMonitor::warning_generation);
        if !self.host_shutdown_requested() {
            let cancelled_generation = self.checkpoint_generation;
            self.checkpoint_generation = None;
            let was_warning = self.phase() == ShutdownPhase::HostShutdownWarning;
            let was_frozen = matches!(self.phase(), ShutdownPhase::Frozen { .. });
            if self.cancel_host_shutdown() {
                app.cancel_host_shutdown_checkpoint();
                self.thaw_after_host_shutdown(app);
            } else if was_warning {
                app.cancel_host_shutdown_checkpoint();
            }
            if was_warning || was_frozen {
                shepr_platform::structured_log!(
                    INFO,
                    event = shutdown.cancel,
                    outcome = Ok,
                    generation = cancelled_generation.map(WarningGeneration::as_u64),
                    "host shutdown cancelled; session saves resumed"
                );
            }
            return;
        }

        match self.phase() {
            ShutdownPhase::Running => {
                if self.begin_host_shutdown_warning() {
                    self.checkpoint_generation = generation;
                    self.freeze_for_host_shutdown(app);
                }
            }
            ShutdownPhase::HostShutdownWarning => {
                if self.checkpoint_generation != generation {
                    // Void both an unclaimed result and the host ticket of an
                    // in-flight save: neither captured this refreshed warning.
                    app.cancel_host_shutdown_checkpoint();
                    self.checkpoint_generation = generation;
                    self.freeze_for_host_shutdown(app);
                } else if app.host_shutdown_checkpoint_result_ready() {
                    self.freeze_for_host_shutdown(app);
                }
            }
            ShutdownPhase::Frozen { .. } => {
                if self.frozen_warning_generation() != generation
                    && self.restart_host_shutdown_warning()
                {
                    app.thaw_session_saves();
                    self.checkpoint_generation = generation;
                    self.freeze_for_host_shutdown(app);
                }
            }
            ShutdownPhase::Stopping => {}
        }
    }

    fn freeze_for_host_shutdown(&mut self, app: &mut app::App) {
        // Only the warning arms of sync_host_shutdown_freeze enter here;
        // the checkpoint result below is the prerequisite for freezing.
        let generation = self.checkpoint_generation;
        if !app.host_shutdown_checkpoint_result_ready() {
            app.request_host_shutdown_checkpoint();
        }
        // Stopped persistence completes synchronously and sends no wake.
        let Some(outcome) = app.take_host_shutdown_checkpoint_result() else {
            return;
        };
        if outcome == app::HostCheckpointOutcome::Unsaved {
            shepr_platform::structured_log!(
                WARN,
                event = shutdown.freeze,
                outcome = Unavailable,
                generation = generation.map(WarningGeneration::as_u64),
                "host shutdown checkpoint unavailable; freezing session saves"
            );
        } else {
            shepr_platform::structured_log!(
                INFO,
                event = shutdown.freeze,
                outcome = Ok,
                generation = generation.map(WarningGeneration::as_u64),
                "host shutdown checkpoint saved; freezing session saves"
            );
        }
        app.freeze_session_saves();
        if let (Some(monitor), Some(generation)) = (self.monitor.as_ref(), generation) {
            monitor.release_delay_lock(generation);
        }
        self.phase = ShutdownPhase::Frozen { generation };
    }

    fn thaw_after_host_shutdown(&mut self, app: &mut app::App) {
        app.resume_session_saves_after_cancel();
    }
}

// Transport draining and checkpointed pane-exit replay stay in the event-loop
// coordinator: they order client replies, internal events and the final save.
// The lifecycle owns phase and host checkpoint policy, not those event queues.
impl HeadlessServer {
    /// Marks terminal server shutdown from any quit source.
    pub(super) fn initiate_shutdown(&mut self) {
        // A request that named its reason wins; otherwise a host shutdown
        // explains the stop, and anything else is the loop ending on its own.
        let fallback = if self.lifecycle.host_shutdown_requested() {
            shepr_api::StopReason::HostShutdown
        } else {
            shepr_api::StopReason::EventLoopExit
        };
        if !self.lifecycle.begin_stopping(fallback) {
            return;
        }
        let reason = self
            .lifecycle
            .stop_signal()
            .reason()
            .unwrap_or(shepr_api::StopReason::EventLoopExit);
        shepr_platform::structured_log!(
            INFO,
            event = server.shutdown,
            outcome = Started,
            cause = %reason,
            "server shutdown initiated"
        );

        // Hand every held reply to its client's FIFO control lane before
        // shutdown cleanup queues the notice. The flush barrier must cover the
        // notice without making earlier replies unreachable to clients that
        // leave when they read it.
        self.release_endpoint_replies(ReleaseMode::Shutdown);
    }

    /// Completes the shutdown sequence, answer every outstanding API request,
    /// and close client connections after their shutdown frames are flushed.
    ///
    /// The socket file is not removed here but in `release_socket_after_save`,
    /// once the session is on disk: while it exists a new `shepr` sees this
    /// server and does not start a daemon that would restore the previous
    /// save, or exit on the still-bound socket and leave the user waiting out
    /// the startup timeout.
    pub(super) async fn complete_shutdown(&mut self) -> Result<(), UnexpectedPhase> {
        // Completing is only legal once `initiate_shutdown` marked the server
        // stopping. The run loop checks the phase before calling; this guard
        // refuses a premature completion before any transport is closed. Keep
        // this guard at the destructive boundary rather than relying on its
        // current caller: direct lifecycle drivers also invoke this method.
        self.lifecycle
            .require_phase(ShutdownStep::CompleteShutdown, ShutdownPhase::Stopping)?;
        shepr_platform::structured_log!(
            INFO,
            event = server.shutdown,
            outcome = Pending,
            "completing server shutdown"
        );
        // A client whose outbox already closed leaves first, so a request it
        // left buffered finds no registered client and is dropped with it.
        self.reap_closed_clients();
        self.reject_late_client_connections().await;

        let shutdown_msg = ServerMessage::server_shutdown();
        self.send_to_all_clients(&shutdown_msg);
        self.queue_shutdown_flushes();

        // Close the request channel and answer what is left in it.
        self.reject_queued_api_requests_for_shutdown();

        // Close all client connections.
        self.clients.clear();
        self.await_shutdown_flushes().await;

        Ok(())
    }

    fn queue_shutdown_flushes(&mut self) {
        let flushes = self
            .clients
            .values()
            .map(|client| client.outbox.flush_barrier())
            .collect::<Vec<_>>();
        self.shutdown_flushes.extend(flushes);
    }

    /// Waits for client writers to flush their shutdown frames, bounded by
    /// one shared deadline: a writer stuck in a socket write to a client that
    /// stopped reading must not hold server shutdown forever.
    async fn await_shutdown_flushes(&mut self) {
        // headless-clock-sample-ok: the bound measures real socket flushes
        // from when this wait begins, not from the loop's earlier sample.
        let deadline = tokio::time::Instant::now() + SHUTDOWN_FLUSH_TIMEOUT;
        for flush in std::mem::take(&mut self.shutdown_flushes) {
            match tokio::time::timeout_at(deadline, flush).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    debug!("client writer exited before acknowledging shutdown flush");
                }
                Err(_) => {
                    shepr_platform::structured_log!(
                        WARN,
                        event = server.client_flush,
                        outcome = Timeout,
                        timeout_ms = SHUTDOWN_FLUSH_TIMEOUT.as_millis(),
                        "client writers did not flush shutdown frames in time; closing anyway"
                    );
                    break;
                }
            }
        }
    }

    /// The shutdown half of the server's one resource order: the final save
    /// is on disk, then the lease is retired, then the socket goes, so a
    /// launcher that sees the socket vanish and starts a daemon meets a free
    /// lease. Retiring the writer waits for any save still in flight before
    /// releasing its lease, so the successor is never sent into a lease held
    /// by this server's retiring writer. Dropping the API handle removes the
    /// socket file. Every exit runs this, including error and unwind exits
    /// through `Drop`; each step is idempotent.
    pub(super) fn release_socket_after_save(&mut self) {
        self.release_socket_after_save_observed(|| {});
    }

    /// [`Self::release_socket_after_save`], running `before_socket_removal`
    /// at the moment the server socket is about to go, which is when a
    /// launcher watching for it would act. Tests observe the lease there.
    pub(super) fn release_socket_after_save_observed(
        &mut self,
        before_socket_removal: impl FnOnce(),
    ) {
        // An exit that never reached its final save (the run errored before
        // the loop, or the server was dropped) still owes a waiting stop
        // request an answer, and an empty one would count as an accepted stop.
        // This is a no-op after a final save has published its result. The
        // answer is written while the socket is still up, before the lease
        // and socket go.
        if self
            .lifecycle
            .stop_signal()
            .complete_unfinished_final_save(UNFINISHED_FINAL_SAVE_MESSAGE)
            && !self
                .lifecycle
                .stop_signal()
                .wait_for_stop_answers(STOP_ANSWER_WAIT)
        {
            debug!("a stop request's answer was not written before the server exit");
        }
        self.app.retire_session_writer();
        before_socket_removal();
        drop(self.api_server.take());
    }
}

#[cfg(test)]
impl ShutdownLifecycle {
    pub(super) fn test_warning_monitor(
        &mut self,
    ) -> tokio::sync::watch::Receiver<Option<WarningGeneration>> {
        let (monitor, checkpoints) =
            HostShutdownMonitor::test_warning(Arc::clone(&self.host_shutdown_request));
        self.monitor = Some(monitor);
        checkpoints
    }

    pub(super) fn test_refresh_warning(&self) {
        self.monitor
            .as_ref()
            .expect("test monitor")
            .test_refresh_warning();
    }
}

#[cfg(test)]
mod phase_tests {
    use super::*;
    use crate::test_support::WorkspaceFixture as _;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn host_shutdown_checkpoint_freezes_and_cancellation_thaws() {
        let config = shepr_config::ServerConfig::default();
        let mut harness = crate::app::App::new(&config);
        harness.test_state_mut().test_set_workspaces(vec![
            shepr_mux::workspace::Workspace::test_new("host-checkpoint"),
        ]);
        let (mut app, outputs) = harness.into_parts();
        let mut lifecycle = ShutdownLifecycle::new(Arc::default());
        let completed = outputs.save_finished_signal();
        lifecycle
            .host_shutdown_request_flag()
            .store(true, Ordering::Release);
        lifecycle.sync_host_shutdown_freeze(&mut app);
        assert_eq!(lifecycle.phase(), ShutdownPhase::HostShutdownWarning);
        tokio::time::timeout(std::time::Duration::from_secs(5), completed.notified())
            .await
            .expect("checkpoint completed");
        app.reap_finished_session_save();
        lifecycle.sync_host_shutdown_freeze(&mut app);
        assert_eq!(
            lifecycle.phase(),
            ShutdownPhase::Frozen { generation: None }
        );
        assert!(
            shepr_mux::persist::session_path(app.test_paths().data_dir())
                .try_exists()
                .expect("stat the session file")
        );
        lifecycle
            .host_shutdown_request_flag()
            .store(false, Ordering::Release);
        lifecycle.sync_host_shutdown_freeze(&mut app);
        assert_eq!(lifecycle.phase(), ShutdownPhase::Running);
        assert!(app.state().session_dirty());
    }

    #[test]
    fn cancelling_a_warning_only_thaws_a_completed_checkpoint() {
        let mut lifecycle = ShutdownLifecycle::new(Arc::default());
        assert!(lifecycle.begin_host_shutdown_warning());
        assert!(!lifecycle.cancel_host_shutdown());
        assert_eq!(lifecycle.phase(), ShutdownPhase::Running);
        let generation = Some(WarningGeneration(7));
        lifecycle.phase = ShutdownPhase::Frozen { generation };
        assert_eq!(lifecycle.frozen_warning_generation(), generation);
        assert!(lifecycle.cancel_host_shutdown());
        assert_eq!(lifecycle.phase(), ShutdownPhase::Running);
        assert!(!lifecycle.cancel_host_shutdown());
    }

    #[test]
    fn restarting_a_frozen_warning_discards_its_generation() {
        let mut lifecycle = ShutdownLifecycle::new(Arc::default());
        lifecycle.phase = ShutdownPhase::Frozen {
            generation: Some(WarningGeneration(7)),
        };
        assert!(lifecycle.restart_host_shutdown_warning());
        assert_eq!(lifecycle.phase(), ShutdownPhase::HostShutdownWarning);
        assert_eq!(lifecycle.frozen_warning_generation(), None);
        assert!(!lifecycle.restart_host_shutdown_warning());
    }

    #[test]
    fn shutdown_refusal_is_safe_to_construct_before_the_stopping_transition() {
        let mut lifecycle = ShutdownLifecycle::new(Arc::default());
        let before = lifecycle.shutdown_error();
        assert!(lifecycle.begin_stopping(shepr_api::StopReason::EventLoopExit));
        assert_eq!(before, lifecycle.shutdown_error());
    }

    #[test]
    fn a_test_server_holds_the_data_directory_lease() {
        // The precondition the test below relies on: a test server holds the
        // lease, so a probe while it is held really does fail.
        let server = super::super::tests::test_headless_server();
        let data_dir = server.app.test_paths().data_dir().to_path_buf();
        assert!(shepr_mux::persist::DataDirLease::acquire(&data_dir).is_err());
        drop(server);
    }

    #[test]
    fn the_lease_is_free_by_the_time_the_socket_goes() {
        // What a launcher that sees the server socket vanish and starts a
        // daemon would meet, observed at exactly that moment. `Drop` and
        // every stop path run this same sequence.
        let mut server = super::super::tests::test_headless_server();
        let data_dir = server.app.test_paths().data_dir().to_path_buf();
        let socket = server
            .app
            .test_paths()
            .server_address()
            .socket()
            .to_path_buf();
        let (tx, _rx) = mpsc::channel(crate::limits::API_REQUEST_CHANNEL_CAPACITY);
        server.api_server = Some(
            shepr_api::start_server(
                tx,
                Arc::clone(server.lifecycle.stop_signal()),
                server.app.test_paths(),
                server.client_shell_boot_id.clone(),
            )
            .expect("real server socket"),
        );
        let mut lease_free = None;
        let mut socket_present = None;
        server.release_socket_after_save_observed(|| {
            socket_present = Some(socket.try_exists().is_ok_and(|present| present));
            lease_free = Some(shepr_mux::persist::DataDirLease::acquire(&data_dir).is_ok());
        });
        assert_eq!(socket_present, Some(true), "probed before the socket went");
        assert_eq!(lease_free, Some(true), "no lease may outlive the socket");
    }

    #[tokio::test]
    async fn completing_shutdown_before_it_began_is_refused() {
        let mut server = super::super::tests::test_headless_server();
        let error = server
            .complete_shutdown()
            .await
            .expect_err("a running server cannot complete shutdown");
        assert_eq!(error.step, ShutdownStep::CompleteShutdown);
        assert_eq!(error.expected, ShutdownPhase::Stopping);
        assert_eq!(error.actual, ShutdownPhase::Running);
        assert_eq!(server.lifecycle.phase(), ShutdownPhase::Running);
    }
}
