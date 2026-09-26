use super::*;

/// Session-save freeze held from a host shutdown warning until the shutdown
/// completes or is cancelled.
pub(super) struct HostShutdownFreeze {
    /// `policy.persist_session` before the freeze turned it off.
    pub(super) persist_session: bool,
    /// The warning this checkpoint answered. A cancellation followed quickly
    /// by another warning may never expose `requested = false` to the loop.
    generation: Option<u64>,
}

impl HeadlessServer {
    pub(super) fn start_host_shutdown_monitor(&mut self) {
        let quit_notify = self.server_event_tx.clone();
        self.host_shutdown_monitor = Some(crate::platform::HostShutdownMonitor::start(
            Arc::clone(&self.host_shutdown_requested),
            move || {
                let _ = quit_notify.try_send(ServerEvent::QuitSignal);
            },
        ));
    }

    /// Answers host shutdown warnings and their cancellation.
    ///
    /// logind announces a shutdown with `PrepareForShutdown(true)` and waits
    /// for delay locks before it starts killing processes; it may also call the
    /// shutdown off again with `PrepareForShutdown(false)`. The server must not
    /// exit on the warning (it would be gone if the shutdown is cancelled), and
    /// must not let the real shutdown's pane kills reach the saved session
    /// (they would be restored as closed). So:
    ///
    /// - Warning (`host_shutdown_requested` set, not frozen): write a
    ///   checkpoint synchronously, then freeze saving by turning
    ///   `policy.persist_session` off. Every save path (debounced, pane-exit
    ///   checkpoint, final) checks that flag. The server keeps running and
    ///   keeps applying events; only the disk is frozen.
    /// - Termination: SIGTERM/SIGINT/SIGHUP (or `server stop`) takes the usual
    ///   quit path, and the final save writes nothing, so the checkpoint stands.
    /// - Cancellation (`host_shutdown_requested` cleared while frozen): thaw,
    ///   restoring `persist_session` and marking the session dirty so the
    ///   current state is saved again.
    ///
    /// The monitor (`platform::shutdown`) reports both warning and cancellation;
    /// the checkpoint releases its delay inhibitor so logind can proceed.
    pub(super) fn sync_host_shutdown_freeze(&mut self, _now: Instant) {
        let requested = self.host_shutdown_requested.load(Ordering::Acquire);
        if self.host_shutdown_freeze.is_none() {
            if requested && !self.shutting_down {
                self.freeze_for_host_shutdown();
            }
            return;
        };
        if !requested {
            self.thaw_after_host_shutdown();
        } else if let (Some(freeze), Some(monitor)) = (
            self.host_shutdown_freeze.as_ref(),
            self.host_shutdown_monitor.as_ref(),
        ) && freeze.generation != Some(monitor.warning_generation())
        {
            let persist_session = freeze.persist_session;
            self.host_shutdown_freeze = None;
            self.app.policy.persist_session = persist_session;
            self.freeze_for_host_shutdown();
        }
    }

    fn freeze_for_host_shutdown(&mut self) {
        info!("host shutdown announced; checkpointing the session and freezing saves");
        let generation = self
            .host_shutdown_monitor
            .as_ref()
            .map(crate::platform::HostShutdownMonitor::warning_generation);
        let persist_session = self.app.policy.persist_session;
        if persist_session {
            self.app.save_session_now();
        }
        self.app.policy.persist_session = false;
        self.app.session_save_deadline = None;
        if let (Some(monitor), Some(generation)) = (self.host_shutdown_monitor.as_ref(), generation)
        {
            monitor.release_delay_lock(generation);
        }
        self.host_shutdown_freeze = Some(HostShutdownFreeze {
            persist_session,
            generation,
        });
    }

    fn thaw_after_host_shutdown(&mut self) {
        let Some(freeze) = self.host_shutdown_freeze.take() else {
            return;
        };
        info!("host shutdown cancelled; resuming session saves");
        self.app.policy.persist_session = freeze.persist_session;
        self.app.state.mark_session_dirty();
    }
    /// Initiates graceful shutdown.
    pub(super) fn initiate_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        info!("server shutdown initiated");
        self.shutting_down = true;

        // Clear client-local host graphics, then send ServerShutdown to all connected clients.
        let shutdown_msg = ServerMessage::ServerShutdown {
            reason: Some("server is shutting down".to_owned()),
        };
        self.send_to_all_clients(&shutdown_msg);

        // Give client writer threads a moment to flush the shutdown message.
        // A short sleep ensures the message is written to the socket before
        // we close the connections.
        std::thread::sleep(Duration::from_millis(50));

        // Signal the main loop to exit.
        self.should_quit.store(true, Ordering::Release);
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
        info!("completing server shutdown");
        self.reject_late_client_connections().await;

        // Send ServerShutdown to all remaining clients.
        if !self.clients.is_empty() {
            let shutdown_msg = ServerMessage::ServerShutdown {
                reason: Some("server is shutting down".to_owned()),
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
