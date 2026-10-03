use super::HeadlessServer;
use crate::app;
use crate::server::outbox::ReleaseMode;
use shepr_protocol::ServerMessage;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, info, warn};

use crate::limits::SHUTDOWN_FLUSH_TIMEOUT;

mod host_shutdown;
use host_shutdown::HostShutdownMonitor;

/// The server lifecycle states that can affect saves or request handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShutdownPhase {
    Running,
    HostShutdownWarning,
    Frozen,
    Stopping,
}

/// Lifecycle operation names kept typed until an error is presented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShutdownStep {
    FreezeForHostShutdown,
    CompleteShutdown,
}

impl std::fmt::Display for ShutdownStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FreezeForHostShutdown => f.write_str("freezing for host shutdown"),
            Self::CompleteShutdown => f.write_str("completing shutdown"),
        }
    }
}

/// A lifecycle step was asked for from a phase it does not start from. The
/// step is refused and the phase left as it was, so an illegal transition
/// never lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct UnexpectedPhase {
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

/// Session-save freeze held from a host shutdown warning until shutdown
/// completes or is cancelled.
pub(super) struct HostShutdownFreeze {
    /// Whether session persistence was active before the freeze.
    pub(super) persist_session: bool,
    /// The warning this checkpoint answered. A cancellation followed quickly
    /// by another warning may never expose `requested = false` to the loop.
    generation: Option<WarningGeneration>,
}

impl HostShutdownFreeze {
    pub(super) fn restored_policy(&self) -> crate::app::AppPolicy {
        if self.persist_session {
            crate::app::AppPolicy::Production
        } else {
            crate::app::AppPolicy::Suspended
        }
    }
}

/// Owns the server's lifecycle phase and the asynchronous request latches that
/// feed it. The latches are written by signal, API and logind threads; the
/// event loop is the sole owner of phase transitions and session policy.
pub(super) struct ShutdownLifecycle {
    phase: ShutdownPhase,
    monitor: Option<HostShutdownMonitor>,
    freeze: Option<HostShutdownFreeze>,
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
            freeze: None,
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

    /// Moves a checkpointed warning to `Frozen`. Only a warning can freeze;
    /// from any other phase the freeze is refused and nothing changes.
    pub(super) fn finish_host_shutdown_freeze(
        &mut self,
        freeze: HostShutdownFreeze,
    ) -> Result<(), UnexpectedPhase> {
        self.require_phase(
            ShutdownStep::FreezeForHostShutdown,
            ShutdownPhase::HostShutdownWarning,
        )?;
        self.freeze = Some(freeze);
        self.phase = ShutdownPhase::Frozen;
        Ok(())
    }

    /// Cancels either a warning not yet checkpointed or a completed freeze.
    /// A returned freeze needs its saved policy restored by the caller.
    pub(super) fn cancel_host_shutdown(&mut self) -> Option<HostShutdownFreeze> {
        match self.phase {
            ShutdownPhase::HostShutdownWarning => {
                self.phase = ShutdownPhase::Running;
                None
            }
            ShutdownPhase::Frozen => {
                self.phase = ShutdownPhase::Running;
                self.freeze.take()
            }
            ShutdownPhase::Running | ShutdownPhase::Stopping => None,
        }
    }

    /// Starts a fresh checkpoint after logind issued another warning before
    /// the loop observed cancellation of the previous one.
    pub(super) fn restart_host_shutdown_warning(&mut self) -> Option<HostShutdownFreeze> {
        if self.phase != ShutdownPhase::Frozen {
            return None;
        }
        self.phase = ShutdownPhase::HostShutdownWarning;
        self.freeze.take()
    }

    pub(super) fn frozen_session_policy(&self) -> Option<bool> {
        match self.phase {
            ShutdownPhase::Frozen | ShutdownPhase::Stopping => {
                self.freeze.as_ref().map(|freeze| freeze.persist_session)
            }
            ShutdownPhase::Running | ShutdownPhase::HostShutdownWarning => None,
        }
    }

    pub(super) fn frozen_warning_generation(&self) -> Option<WarningGeneration> {
        if self.phase == ShutdownPhase::Frozen {
            self.freeze.as_ref().and_then(|freeze| freeze.generation)
        } else {
            None
        }
    }

    pub(super) fn begin_stopping(&mut self) -> bool {
        if self.phase == ShutdownPhase::Stopping {
            return false;
        }
        self.phase = ShutdownPhase::Stopping;
        self.stop_signal.request();
        true
    }

    /// The canonical rejection of a JSON API request selected after the
    /// server entered its terminal stopping phase. An endpoint command gets
    /// `EndpointError::ShuttingDown` instead.
    pub(super) fn shutdown_error(&self) -> shepr_api::error::ApiError {
        assert_eq!(self.phase, ShutdownPhase::Stopping);
        shepr_api::error::ApiError::new(
            shepr_api::error::ApiErrorCode::ServerUnavailable,
            shepr_protocol::ShutdownReason::Stopping.to_string(),
        )
    }
}

impl ShutdownLifecycle {
    pub(super) fn start_host_shutdown_monitor(&mut self, wake: &Arc<tokio::sync::Notify>) {
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

        if !self.host_shutdown_requested() {
            let was_warning = self.phase() == ShutdownPhase::HostShutdownWarning;
            if let Some(freeze) = self.cancel_host_shutdown() {
                app.cancel_host_shutdown_checkpoint();
                self.thaw_after_host_shutdown(app, &freeze);
            } else if was_warning {
                app.cancel_host_shutdown_checkpoint();
            }
            return;
        }

        match self.phase() {
            ShutdownPhase::Running => {
                if self.begin_host_shutdown_warning() {
                    self.freeze_for_host_shutdown(app);
                }
            }
            ShutdownPhase::HostShutdownWarning => {
                if !app.policy.persists_session() || app.host_shutdown_checkpoint_result_ready() {
                    self.freeze_for_host_shutdown(app);
                }
            }
            ShutdownPhase::Frozen => {
                let generation = self
                    .monitor
                    .as_ref()
                    .and_then(HostShutdownMonitor::warning_generation);
                if self.frozen_warning_generation() != generation
                    && let Some(freeze) = self.restart_host_shutdown_warning()
                {
                    app.thaw_session_saves(freeze.restored_policy());
                    self.freeze_for_host_shutdown(app);
                }
            }
            ShutdownPhase::Stopping => {}
        }
    }

    fn freeze_for_host_shutdown(&mut self, app: &mut app::App) {
        // Checked before any side effect: a refused freeze must leave the save
        // policy and logind's delay lock as they were.
        if let Err(error) = self.require_phase(
            ShutdownStep::FreezeForHostShutdown,
            ShutdownPhase::HostShutdownWarning,
        ) {
            tracing::error!(%error, "refusing the host shutdown freeze");
            return;
        }
        info!("host shutdown announced; checkpointing the session and freezing saves");
        let generation = self
            .monitor
            .as_ref()
            .and_then(HostShutdownMonitor::warning_generation);
        let persist_session = app.policy.persists_session();
        if persist_session {
            let Some(saved) = app.take_host_shutdown_checkpoint_result() else {
                app.request_host_shutdown_checkpoint();
                return;
            };
            if !saved {
                warn!("host shutdown checkpoint failed repeatedly; releasing the delay lock");
            }
        }
        app.freeze_session_saves();
        if let (Some(monitor), Some(generation)) = (self.monitor.as_ref(), generation) {
            monitor.release_delay_lock(generation);
        }
        if let Err(error) = self.finish_host_shutdown_freeze(HostShutdownFreeze {
            persist_session,
            generation,
        }) {
            tracing::error!(%error, "host shutdown freeze did not land");
        }
    }

    fn thaw_after_host_shutdown(&mut self, app: &mut app::App, freeze: &HostShutdownFreeze) {
        info!("host shutdown cancelled; resuming session saves");
        app.thaw_session_saves(freeze.restored_policy());
        app.state.mark_session_dirty();
    }
}

// Transport draining and checkpointed pane-exit replay stay in the event-loop
// coordinator: they order client replies, internal events and the final save.
// The lifecycle owns phase and host checkpoint policy, not those event queues.
impl HeadlessServer {
    /// Marks terminal server shutdown from any quit source.
    pub(super) fn initiate_shutdown(&mut self) {
        if !self.lifecycle.begin_stopping() {
            return;
        }
        info!("server shutdown initiated");

        // Resolve worker slots and hand every held reply to its client's
        // FIFO control lane before shutdown cleanup queues the notice. The
        // flush barrier must cover the notice without making earlier replies
        // unreachable to clients that leave when they read it.
        self.resolve_pending_endpoint_replies_for_shutdown();
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
    pub(super) async fn complete_shutdown(&mut self) -> io::Result<()> {
        // Completing is only legal once `initiate_shutdown` marked the server
        // stopping; this step settles queued requests before notifying clients.
        self.lifecycle
            .require_phase(ShutdownStep::CompleteShutdown, ShutdownPhase::Stopping)
            .map_err(io::Error::other)?;
        info!("completing server shutdown");
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
                    warn!(
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
        self.app.retire_session_writer();
        before_socket_removal();
        drop(self.api_server.take());
    }
}

#[cfg(test)]
impl ShutdownLifecycle {
    pub(super) fn has_monitor(&self) -> bool {
        self.monitor.is_some()
    }

    pub(super) fn set_frozen_session_policy_for_test(&mut self, persist_session: bool) {
        if let Some(freeze) = self.freeze.as_mut() {
            freeze.persist_session = persist_session;
        }
    }
}

#[cfg(test)]
mod phase_tests {
    use super::*;
    use tokio::sync::mpsc;

    fn freeze() -> HostShutdownFreeze {
        HostShutdownFreeze {
            persist_session: true,
            generation: None,
        }
    }

    #[tokio::test]
    async fn host_shutdown_freeze_waits_for_monitor_cancellation() {
        let config = shepr_config::ServerConfig::default();
        let mut app = crate::app::App::new(&config, crate::app::AppPolicy::Suspended);
        let mut lifecycle = ShutdownLifecycle::new(Arc::default());
        lifecycle
            .host_shutdown_request_flag()
            .store(true, Ordering::Release);
        lifecycle.sync_host_shutdown_freeze(&mut app);
        assert_eq!(lifecycle.phase(), ShutdownPhase::Frozen);

        lifecycle.sync_host_shutdown_freeze(&mut app);
        assert_eq!(lifecycle.phase(), ShutdownPhase::Frozen);
        assert!(lifecycle.host_shutdown_requested());

        lifecycle.sync_host_shutdown_freeze(&mut app);
        assert_eq!(lifecycle.phase(), ShutdownPhase::Frozen);
        lifecycle
            .host_shutdown_request_flag()
            .store(false, Ordering::Release);
        lifecycle.sync_host_shutdown_freeze(&mut app);
        assert_eq!(lifecycle.phase(), ShutdownPhase::Running);
        assert!(!lifecycle.host_shutdown_requested());
        // No monitor ran before the warning, so none was started by the thaw.
        assert!(!lifecycle.has_monitor());
    }

    #[test]
    fn freeze_is_refused_outside_a_warning_and_leaves_the_phase() {
        let mut lifecycle = ShutdownLifecycle::new(Arc::default());
        let refused = lifecycle
            .finish_host_shutdown_freeze(freeze())
            .expect_err("a running server cannot freeze");
        assert_eq!(refused.actual, ShutdownPhase::Running);
        assert_eq!(lifecycle.phase(), ShutdownPhase::Running);
        assert_eq!(lifecycle.frozen_session_policy(), None);

        assert!(lifecycle.begin_stopping());
        assert!(lifecycle.finish_host_shutdown_freeze(freeze()).is_err());
        assert_eq!(lifecycle.phase(), ShutdownPhase::Stopping);
    }

    #[test]
    fn freeze_lands_from_a_warning() {
        let mut lifecycle = ShutdownLifecycle::new(Arc::default());
        assert!(lifecycle.begin_host_shutdown_warning());
        lifecycle
            .finish_host_shutdown_freeze(freeze())
            .expect("a warning freezes");
        assert_eq!(lifecycle.phase(), ShutdownPhase::Frozen);
        assert_eq!(lifecycle.frozen_session_policy(), Some(true));
    }

    #[test]
    fn a_test_server_holds_the_data_directory_lease() {
        // The precondition the test below relies on: a test server holds the
        // lease, so a probe while it is held really does fail.
        let server = super::super::tests::test_headless_server();
        let data_dir = server.app.paths.data_dir().to_path_buf();
        assert!(shepr_mux::persist::DataDirLease::acquire(&data_dir).is_err());
        drop(server);
    }

    #[test]
    fn the_lease_is_free_by_the_time_the_socket_goes() {
        // What a launcher that sees the server socket vanish and starts a
        // daemon would meet, observed at exactly that moment. `Drop` and
        // every stop path run this same sequence.
        let mut server = super::super::tests::test_headless_server();
        let data_dir = server.app.paths.data_dir().to_path_buf();
        let socket = server.app.paths.server_address().socket().to_path_buf();
        let (tx, _rx) = mpsc::channel(crate::limits::API_REQUEST_CHANNEL_CAPACITY);
        server.api_server = Some(
            shepr_api::start_server(
                tx,
                Arc::clone(server.lifecycle.stop_signal()),
                &server.app.paths,
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
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(server.lifecycle.phase(), ShutdownPhase::Running);
    }
}
