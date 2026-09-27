use super::*;

/// The server lifecycle states that can affect saves or request handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShutdownPhase {
    Running,
    HostShutdownWarning,
    Frozen,
    Stopping,
}

/// Session-save freeze held from a host shutdown warning until shutdown
/// completes or is cancelled.
pub(super) struct HostShutdownFreeze {
    /// Whether session persistence was active before the freeze.
    pub(super) persist_session: bool,
    /// The warning this checkpoint answered. A cancellation followed quickly
    /// by another warning may never expose `requested = false` to the loop.
    generation: Option<u64>,
}

/// Owns the server's lifecycle phase and the asynchronous request latches that
/// feed it. The latches are written by signal, API and logind threads; the
/// event loop is the sole owner of phase transitions and session policy.
pub(super) struct ShutdownLifecycle {
    phase: ShutdownPhase,
    freeze: Option<HostShutdownFreeze>,
    stop_request: Arc<AtomicBool>,
    host_shutdown_request: Arc<AtomicBool>,
    signal_quit_request: Arc<AtomicBool>,
}

impl ShutdownLifecycle {
    pub(super) fn new(stop_request: Arc<AtomicBool>) -> Self {
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
        self.phase == ShutdownPhase::Stopping
            || app_quit
            || self.stop_request.load(Ordering::Acquire)
    }

    pub(super) fn stop_request_flag(&self) -> &Arc<AtomicBool> {
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

    pub(super) fn finish_host_shutdown_freeze(&mut self, freeze: HostShutdownFreeze) {
        debug_assert_eq!(self.phase, ShutdownPhase::HostShutdownWarning);
        self.freeze = Some(freeze);
        self.phase = ShutdownPhase::Frozen;
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
        self.stop_request.store(true, Ordering::Release);
        true
    }

    /// The canonical rejection used for requests selected after the server
    /// entered its terminal stopping phase.
    pub(super) fn shutdown_error(&self) -> Option<api::schema::ErrorBody> {
        if self.phase == ShutdownPhase::Stopping {
            Some(
                api::error::ApiError::new(
                    api::error::ApiErrorCode::ServerUnavailable,
                    "server is shutting down",
                )
                .into_body(),
            )
        } else {
            None
        }
    }

    #[cfg(test)]
    pub(super) fn set_frozen_session_policy_for_test(&mut self, persist_session: bool) {
        if let Some(freeze) = self.freeze.as_mut() {
            freeze.persist_session = persist_session;
        }
    }
}

impl HeadlessServer {
    pub(super) fn start_host_shutdown_monitor(&mut self) {
        let quit_notify = self.server_event_tx.clone();
        self.host_shutdown_monitor = Some(shepr_platform::HostShutdownMonitor::start(
            Arc::clone(self.lifecycle.host_shutdown_request_flag()),
            move || {
                let _ = quit_notify.try_send(ServerEvent::QuitSignal);
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
            if let Some(freeze) = self.lifecycle.cancel_host_shutdown() {
                self.thaw_after_host_shutdown(&freeze);
            }
            return;
        }

        match self.lifecycle.phase() {
            ShutdownPhase::Running => {
                if self.lifecycle.begin_host_shutdown_warning() {
                    self.freeze_for_host_shutdown();
                }
            }
            ShutdownPhase::HostShutdownWarning => self.freeze_for_host_shutdown(),
            ShutdownPhase::Frozen => {
                let generation = self
                    .host_shutdown_monitor
                    .as_ref()
                    .map(shepr_platform::HostShutdownMonitor::warning_generation);
                if self.lifecycle.frozen_warning_generation() != generation
                    && let Some(freeze) = self.lifecycle.restart_host_shutdown_warning()
                {
                    self.app.policy = if freeze.persist_session {
                        crate::app::AppPolicy::PRODUCTION
                    } else {
                        crate::app::AppPolicy::Suspended
                    };
                    self.freeze_for_host_shutdown();
                }
            }
            ShutdownPhase::Stopping => {}
        }
    }

    fn freeze_for_host_shutdown(&mut self) {
        info!("host shutdown announced; checkpointing the session and freezing saves");
        let generation = self
            .host_shutdown_monitor
            .as_ref()
            .map(shepr_platform::HostShutdownMonitor::warning_generation);
        let persist_session = self.app.policy.persists_session();
        if persist_session {
            self.app.save_session_now();
        }
        self.app.policy = crate::app::AppPolicy::Suspended;
        self.app.session_saver.clear_deadline();
        if let (Some(monitor), Some(generation)) = (self.host_shutdown_monitor.as_ref(), generation)
        {
            monitor.release_delay_lock(generation);
        }
        self.lifecycle
            .finish_host_shutdown_freeze(HostShutdownFreeze {
                persist_session,
                generation,
            });
    }

    fn thaw_after_host_shutdown(&mut self, freeze: &HostShutdownFreeze) {
        info!("host shutdown cancelled; resuming session saves");
        self.app.policy = if freeze.persist_session {
            crate::app::AppPolicy::PRODUCTION
        } else {
            crate::app::AppPolicy::Suspended
        };
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

        // Give client writer threads a moment to flush the shutdown message.
        // A short sleep ensures the message is written to the socket before
        // we close the connections.
        std::thread::sleep(Duration::from_millis(50));

        self.app.state.should_quit = true;
    }

    /// Completes the shutdown sequence: send ServerShutdown to clients, answer
    /// every outstanding API request, and close client connections.
    ///
    /// Socket files are not removed here but in `release_sockets_after_save`,
    /// once the session is on disk: while either socket file exists a new
    /// `shepr` sees this server and does not start a daemon that would restore
    /// the previous save, or exit on the still-bound API socket and leave the
    /// user waiting out the startup timeout.
    pub(super) async fn complete_shutdown(&mut self) -> io::Result<()> {
        debug_assert_eq!(self.lifecycle.phase(), ShutdownPhase::Stopping);
        info!("completing server shutdown");
        self.reject_late_client_connections().await;

        // Send ServerShutdown to all remaining clients.
        if !self.clients.is_empty() {
            let shutdown_msg = ServerMessage::ServerShutdown {
                reason: Some(shepr_protocol::ShutdownReason::Message(
                    "server is shutting down".to_owned(),
                )),
            };
            self.send_to_all_clients(&shutdown_msg);

            // Give writer threads a moment to flush before closing.
            std::thread::sleep(Duration::from_millis(50));
        }

        // Close the request channel and answer what is left in it, then the
        // reads parked on alternate-screen traversals that no loop will drive.
        self.reject_queued_api_requests_for_shutdown();
        self.finish_alt_screen_reads_for_shutdown();

        // Close all client connections.
        self.clients.clear();

        Ok(())
    }

    /// Removes the API socket and then the client socket, after the final
    /// session save.
    pub(super) fn release_sockets_after_save(&mut self) -> io::Result<()> {
        // Dropping the handle removes the API socket file.
        drop(self._api_server.take());
        self.cleanup_sockets()
    }

    /// Removes socket files created by the server.
    pub(super) fn cleanup_sockets(&self) -> io::Result<()> {
        if let Err(err) =
            remove_socket_file_if_owned(&self.client_socket_path, &self.client_socket_identity)
            && err.kind() != io::ErrorKind::NotFound
        {
            warn!(
                path = %self.client_socket_path.display(),
                err = %err,
                "failed to remove client socket on shutdown"
            );
        }
        Ok(())
    }
}
