use super::*;
use endpoint::{
    Lost,
    view::{self, HostBaseline, StartOutcome},
};

/// Shows an endpoint notice and presents the chrome. It decides nothing about what is shown:
/// the choice already says that.
pub(super) fn present_notice(state: &mut ClientState, message: String) {
    state.shell.receive_endpoint_unavailable(message);
    if let Some(frame) = state.shell.compose(
        state.reported_geometry.cols(),
        state.reported_geometry.rows(),
    ) {
        state.present_chrome(frame);
    }
}

/// The notice when the endpoint a move targets disconnects before the move commits.
/// The predicate is fixed UI text; remote diagnostics stay in machine diagnostics, and every
/// raw transport error stays in the log.
fn move_interrupted_notice(label: &str, notice: &str) -> String {
    format!("machine switch interrupted: {label} {notice}")
}

impl ClientLoop {
    /// The loop's one derivation of what is shown and which connections are viewed, run once
    /// per iteration before the loop waits: queued connection failures, a failed or expired
    /// move, starting a move, the move's navigation, its commit, and turning off every viewed
    /// connection nobody wants. Failures come first, so a target lost at its deadline reads
    /// as an interrupted switch rather than a timeout.
    pub(super) fn reconcile(&mut self, now: std::time::Instant) -> Result<(), ClientError> {
        for failure in self.write_stream.take_failures() {
            // `record_failure` removed the connection as it queued this failure; a live
            // connection of another generation is a reconnect this failure must not end.
            if self.write_stream.connection(&failure.endpoint_id).is_some()
                && !self
                    .write_stream
                    .accepts(&failure.endpoint_id, failure.generation)
            {
                continue;
            }
            warn!(
                endpoint = %failure.endpoint_id.storage_key(),
                error = %failure.message,
                "endpoint transport failed"
            );
            if self
                .local_failure_policy
                .ends_client_for(&failure.endpoint_id)
            {
                return Err(ClientError::ConnectionLost(io::Error::new(
                    failure.kind,
                    failure.message,
                )));
            }
            self.endpoint_lost(failure, now)?;
        }
        if let Some(preparing) = self.state.choice.preparing() {
            if let Some(rejection) = preparing.rejection() {
                let rejection = rejection.to_owned();
                self.fail_move(|label| format!("{label}: {rejection}"));
            } else if now >= preparing.deadline() {
                self.fail_move(|label| {
                    format!("{label} did not produce a coherent surface in time")
                });
            }
        }
        let baseline = HostBaseline {
            geometry: view_geometry(&self.state),
            host_focused: self.state.shell.host_focus_baseline(),
            theme: &self.state.host_theme_updates,
        };
        // Sent by a commit later in this turn, not by `turn_on`.
        let focused = baseline.host_focused;
        if let StartOutcome::Abandoned(to) = view::start_move(
            &mut self.state.choice,
            &mut self.write_stream,
            &self.state.shell,
            &baseline,
            &mut self.next_view_serial,
            now,
        ) {
            let message = format!("{} is not ready", self.state.shell.endpoint_label(&to));
            present_notice(&mut self.state, message);
        }
        view::send_focus(&mut self.state.choice, &mut self.write_stream);
        let size = self.state.shell.surface_size(
            self.state.reported_geometry.cols(),
            self.state.reported_geometry.rows(),
        );
        match view::commit_move(
            &mut self.state.choice,
            &mut self.write_stream,
            &mut self.state.shell,
            focused,
            size,
        ) {
            Ok(Some(committed)) => {
                if let Some(previous) = committed.previous {
                    clear_endpoint_host_effects(&mut self.state)?;
                    let cancelled = self.endpoint_commands.retire_lane(&previous);
                    cancel_endpoint_commands(&mut self.state.shell, cancelled);
                }
                let cancelled =
                    self.endpoint_commands
                        .send_next(&committed.shown, &mut self.write_stream, now);
                cancel_endpoint_commands(&mut self.state.shell, cancelled);
                self.state.request_repaint();
                if let Some(frame) = self.state.shell.compose(
                    self.state.reported_geometry.cols(),
                    self.state.reported_geometry.rows(),
                ) {
                    self.state.present_frame(frame);
                }
            }
            Err(reason) => self.fail_move(|label| format!("{label}: {reason}")),
            Ok(None) => {}
        }
        view::release_unwanted(
            &self.state.choice,
            &mut self.write_stream,
            &self.state.shell,
            &mut self.next_view_serial,
        );
        Ok(())
    }

    /// Fails the move being prepared and reports it. `notice` builds the message from the
    /// target's label. The target needs no cleanup: it is no longer wanted, so the release
    /// step of this same turn turns it off.
    fn fail_move(&mut self, notice: impl FnOnce(&str) -> String) {
        if let Some(failed) = self.state.choice.fail_move() {
            let message = notice(self.state.shell.endpoint_label(&failed.to));
            present_notice(&mut self.state, message);
        }
    }

    /// Handles one lost connection: supervisor and diagnostic, the choice, its command lane,
    /// the shell status, then the notice for what the loss meant to the choice.
    fn endpoint_lost(
        &mut self,
        failure: endpoint::EndpointTransportFailure,
        now: std::time::Instant,
    ) -> Result<(), ClientError> {
        let id = &failure.endpoint_id;
        self.supervisors.disconnected(id, failure.generation, now);
        let notice = endpoint_disconnect_notice(failure.kind);
        let diagnostic = shepr_remote::SshFailureDiagnostic::from_message(failure.message);
        self.state.shell.set_machine_diagnostic(id, &diagnostic);
        let lost = self.state.choice.connection_lost(id);
        let cancelled = self.endpoint_commands.disconnect(id);
        cancel_endpoint_commands(&mut self.state.shell, cancelled);
        self.state.shell.mark_endpoint_disconnected(id);
        let label = self.state.shell.endpoint_label(id);
        match lost {
            Lost::Shown => {
                let message = format!("{label} {notice}");
                present_notice(&mut self.state, message);
                clear_endpoint_host_effects(&mut self.state)?;
            }
            Lost::Target => {
                let message = move_interrupted_notice(label, notice);
                present_notice(&mut self.state, message);
            }
            Lost::Unrelated => {
                if let Some(frame) = self.state.shell.compose(
                    self.state.reported_geometry.cols(),
                    self.state.reported_geometry.rows(),
                ) {
                    self.state.present_chrome(frame);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interrupted_machine_switch_names_the_machine_and_reads_as_one_sentence() {
        assert_eq!(
            move_interrupted_notice("buildbox", "connection was lost; reconnecting"),
            "machine switch interrupted: buildbox connection was lost; reconnecting"
        );
    }
}
