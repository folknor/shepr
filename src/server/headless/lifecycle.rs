use super::*;

impl HeadlessServer {
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
