use super::*;

use crate::limits::SHUTDOWN_FLUSH_TIMEOUT;

/// The server lifecycle states that can affect saves or request handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShutdownPhase {
    Running,
    HostShutdownWarning,
    Frozen,
    Stopping,
}

/// A lifecycle step was asked for from a phase it does not start from. The
/// step is refused and the phase left as it was, so an illegal transition
/// never lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct UnexpectedPhase {
    pub(super) step: &'static str,
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

/// Session-save freeze held from a host shutdown warning until shutdown
/// completes or is cancelled.
pub(super) struct HostShutdownFreeze {
    /// Whether session persistence was active before the freeze.
    pub(super) persist_session: bool,
    /// The warning this checkpoint answered. A cancellation followed quickly
    /// by another warning may never expose `requested = false` to the loop.
    generation: Option<u64>,
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
    freeze: Option<HostShutdownFreeze>,
    stop_request: Arc<shepr_api::ServerStopSignal>,
    host_shutdown_request: Arc<AtomicBool>,
    signal_quit_request: Arc<AtomicBool>,
}

impl ShutdownLifecycle {
    pub(super) fn new(stop_request: Arc<shepr_api::ServerStopSignal>) -> Self {
        Self {
            phase: ShutdownPhase::Running,
            freeze: None,
            stop_request,
            host_shutdown_request: Arc::new(AtomicBool::new(false)),
            signal_quit_request: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn phase(&self) -> ShutdownPhase {
        self.phase
    }

    /// Whether any quit source has requested termination, or termination has
    /// already begun. `app_quit` is the in-process input path; the atomic latch
    /// is shared with signal and API server-stop handling.
    pub(super) fn stop_requested(&self, app_quit: bool) -> bool {
        self.phase == ShutdownPhase::Stopping || app_quit || self.stop_request.is_requested()
    }

    pub(super) fn stop_signal(&self) -> &Arc<shepr_api::ServerStopSignal> {
        &self.stop_request
    }

    pub(super) fn host_shutdown_request_flag(&self) -> &Arc<AtomicBool> {
        &self.host_shutdown_request
    }

    pub(super) fn signal_quit_request_flag(&self) -> &Arc<AtomicBool> {
        &self.signal_quit_request
    }

    pub(super) fn signal_quit_requested(&self) -> bool {
        self.signal_quit_request.load(Ordering::Acquire)
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
        step: &'static str,
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
            "freezing for host shutdown",
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

    pub(super) fn frozen_warning_generation(&self) -> Option<u64> {
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
        self.stop_request.request();
        true
    }

    /// The canonical rejection used for requests selected after the server
    /// entered its terminal stopping phase.
    pub(super) fn shutdown_error(&self) -> Option<shepr_api::error::ApiError> {
        if self.phase == ShutdownPhase::Stopping {
            Some(shepr_api::error::ApiError::new(
                shepr_api::error::ApiErrorCode::ServerUnavailable,
                "server is shutting down",
            ))
        } else {
            None
        }
    }
}

impl HeadlessServer {
    pub(super) fn start_host_shutdown_monitor(&mut self) {
        let quit_notify = self.server_event_tx.clone();
        self.host_shutdown_monitor = Some(shepr_platform::HostShutdownMonitor::start(
            Arc::clone(self.lifecycle.host_shutdown_request_flag()),
            move || {
                // Only a wakeup: the monitor updates the request flag before
                // calling this, and the loop reads the flag every iteration.
                // A full channel already wakes the loop, and a closed one
                // means the loop has exited.
                let (Ok(())
                | Err(
                    mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_),
                )) = quit_notify.try_send(ServerEvent::QuitSignal);
            },
        ));
    }

    /// Applies warning and cancellation notifications from logind to the
    /// lifecycle state machine. A warning checkpoints before freezing saves;
    /// cancellation thaws and marks the live session dirty again.
    pub(super) fn sync_host_shutdown_freeze(&mut self, _now: Instant) {
        if self.lifecycle.phase() == ShutdownPhase::Stopping {
            return;
        }

        if !self.lifecycle.host_shutdown_requested() {
            let was_warning = self.lifecycle.phase() == ShutdownPhase::HostShutdownWarning;
            if let Some(freeze) = self.lifecycle.cancel_host_shutdown() {
                self.app.cancel_host_shutdown_checkpoint();
                self.thaw_after_host_shutdown(&freeze);
            } else if was_warning {
                self.app.cancel_host_shutdown_checkpoint();
            }
            return;
        }

        match self.lifecycle.phase() {
            ShutdownPhase::Running => {
                if self.lifecycle.begin_host_shutdown_warning() {
                    self.freeze_for_host_shutdown();
                }
            }
            ShutdownPhase::HostShutdownWarning => {
                if !self.app.policy.persists_session()
                    || self.app.host_shutdown_checkpoint_result_ready()
                {
                    self.freeze_for_host_shutdown();
                }
            }
            ShutdownPhase::Frozen => {
                let generation = self
                    .host_shutdown_monitor
                    .as_ref()
                    .map(shepr_platform::HostShutdownMonitor::warning_generation);
                if self.lifecycle.frozen_warning_generation() != generation
                    && let Some(freeze) = self.lifecycle.restart_host_shutdown_warning()
                {
                    self.app.policy = freeze.restored_policy();
                    self.freeze_for_host_shutdown();
                }
            }
            ShutdownPhase::Stopping => {}
        }
    }

    fn freeze_for_host_shutdown(&mut self) {
        // Checked before any side effect: a refused freeze must leave the save
        // policy and logind's delay lock as they were.
        if let Err(error) = self.lifecycle.require_phase(
            "freezing for host shutdown",
            ShutdownPhase::HostShutdownWarning,
        ) {
            tracing::error!(%error, "refusing the host shutdown freeze");
            return;
        }
        info!("host shutdown announced; checkpointing the session and freezing saves");
        let generation = self
            .host_shutdown_monitor
            .as_ref()
            .map(shepr_platform::HostShutdownMonitor::warning_generation);
        let persist_session = self.app.policy.persists_session();
        if persist_session {
            let Some(saved) = self.app.take_host_shutdown_checkpoint_result() else {
                self.app.request_host_shutdown_checkpoint();
                return;
            };
            if !saved {
                warn!("host shutdown checkpoint failed repeatedly; releasing the delay lock");
            }
        }
        self.app.policy = crate::app::AppPolicy::Suspended;
        self.app.session_saver.freeze_session_saves();
        if let (Some(monitor), Some(generation)) = (self.host_shutdown_monitor.as_ref(), generation)
        {
            monitor.release_delay_lock(generation);
        }
        if let Err(error) = self
            .lifecycle
            .finish_host_shutdown_freeze(HostShutdownFreeze {
                persist_session,
                generation,
            })
        {
            tracing::error!(%error, "host shutdown freeze did not land");
        }
    }

    fn thaw_after_host_shutdown(&mut self, freeze: &HostShutdownFreeze) {
        info!("host shutdown cancelled; resuming session saves");
        self.app.policy = freeze.restored_policy();
        self.app.state.mark_session_dirty();
    }

    /// Initiates terminal server shutdown from any quit source.
    pub(super) fn initiate_shutdown(&mut self) {
        if !self.lifecycle.begin_stopping() {
            return;
        }
        info!("server shutdown initiated");

        // Clear client-local host graphics, then send ServerShutdown to all connected clients.
        let shutdown_msg = ServerMessage::ServerShutdown {
            reason: Some(shepr_protocol::ShutdownReason::Message(
                "server is shutting down".to_owned(),
            )),
        };
        self.send_to_all_clients(&shutdown_msg);
        self.queue_shutdown_flushes();

        self.app.state.should_quit = true;
    }

    /// Completes the shutdown sequence, answer every outstanding API request,
    /// and close client connections after their shutdown frames are flushed.
    ///
    /// Socket files are not removed here but in `release_sockets_after_save`,
    /// once the session is on disk: while either socket file exists a new
    /// `shepr` sees this server and does not start a daemon that would restore
    /// the previous save, or exit on the still-bound API socket and leave the
    /// user waiting out the startup timeout.
    pub(super) async fn complete_shutdown(&mut self) -> io::Result<()> {
        // Completing is only legal once `initiate_shutdown` has told every
        // client; before that, closing connections would drop them silently.
        self.lifecycle
            .require_phase("completing shutdown", ShutdownPhase::Stopping)
            .map_err(io::Error::other)?;
        info!("completing server shutdown");
        self.reject_late_client_connections().await;

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
            .filter_map(|client| {
                client
                    .writer
                    .as_ref()
                    .map(crate::server::client_transport::ClientWriter::flush)
            })
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

    /// Removes the API socket and then the client socket, after the final
    /// session save.
    pub(super) fn release_sockets_after_save(&mut self) {
        // Dropping the handle removes the API socket file.
        drop(self._api_server.take());
        self.cleanup_sockets();
    }

    /// Removes socket files created by the server. A removal failure is
    /// logged, not returned: this runs on the way out (final shutdown and
    /// `Drop`), where no caller could act on it.
    pub(super) fn cleanup_sockets(&self) {
        if let Err(err) =
            remove_socket_file_if_owned(&self.client_socket_path, &self.client_socket_identity)
            && err.kind() != io::ErrorKind::NotFound
        {
            warn!(
                path = %self.client_socket_path.display(),
                error = %err,
                "failed to remove client socket on shutdown"
            );
        }
    }
}

#[cfg(test)]
impl ShutdownLifecycle {
    pub(super) fn set_frozen_session_policy_for_test(&mut self, persist_session: bool) {
        if let Some(freeze) = self.freeze.as_mut() {
            freeze.persist_session = persist_session;
        }
    }
}

#[cfg(test)]
mod phase_tests {
    use super::*;

    fn freeze() -> HostShutdownFreeze {
        HostShutdownFreeze {
            persist_session: true,
            generation: None,
        }
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
