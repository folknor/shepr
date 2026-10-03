use super::*;
use endpoint::{
    Lost,
    view::{self, HostBaseline, StartOutcome},
};

/// Shows an endpoint notice and presents the chrome. It decides nothing about what is shown:
/// the choice already says that.
pub(super) fn present_notice(state: &mut ClientState, notice: &shell::EndpointNotice) {
    state.shell.receive_endpoint_unavailable(notice);
    state.mark_chrome_dirty();
}

impl ClientLoop {
    /// The loop's one derivation of what is shown and which connections are viewed, run once
    /// per iteration before the loop waits: queued connection failures, a failed or expired
    /// move, starting a move, the move's navigation, its commit, and turning off every viewed
    /// connection nobody wants. Failures come first, so a target lost at its deadline reads
    /// as an interrupted switch rather than a timeout.
    pub(super) fn reconcile(&mut self, now: std::time::Instant) -> Result<(), ClientError> {
        for failure in self.write_stream.take_failures() {
            // `record_failure` removed the connection as it queued this failure, and no
            // connection of another generation can have replaced it yet: a connection is only
            // installed by its supervisor's attempt, the supervisor starts no attempt while its
            // generation is connected, and only `endpoint_lost` below (`disconnected`) re-arms
            // it. So every queued failure ends its endpoint's lane here.
            warn!(
                endpoint = %failure.endpoint_id,
                error = %failure.failure,
                "endpoint transport failed"
            );
            if self
                .local_failure_policy
                .ends_client_for(failure.endpoint_id.policy())
            {
                return Err(ClientError::ConnectionLost(io::Error::new(
                    failure.kind,
                    failure.failure,
                )));
            }
            self.endpoint_lost(&failure, now)?;
        }
        if let Some(preparing) = self.state.shell.endpoints.choice.preparing() {
            if let Some(rejection) = preparing.rejection() {
                let rejection = rejection.to_owned();
                self.fail_move(shell::EndpointNoticeKind::MoveRejected(rejection));
            } else if now >= preparing.deadline() {
                self.fail_move(shell::EndpointNoticeKind::MoveSurfaceTimedOut);
            }
        }
        let host_geometry = self.state.reported_geometry;
        let shell = &mut self.state.shell;
        let theme = &self.state.host_theme_updates;
        // Focus is sent at commit. Geometry and theme are used only if a move is ready to
        // start; deriving the layout on every ordinary event would repeat shell work.
        let focused = shell.host_focus_baseline();
        let baseline = |shell: &ClientShellState| HostBaseline {
            geometry: view_geometry(
                host_geometry,
                shell.surface_size(host_geometry.cols(), host_geometry.rows()),
            ),
            theme,
        };
        if let StartOutcome::Abandoned(to) = view::start_move(
            &mut self.write_stream,
            shell,
            baseline,
            &mut self.next_view_serial,
            now,
        ) {
            present_notice(
                &mut self.state,
                &shell::EndpointNotice::new(to, shell::EndpointNoticeKind::NotReady),
            );
        }
        view::send_focus(
            &mut self.state.shell.endpoints.choice,
            &mut self.write_stream,
        );
        match view::commit_move(&mut self.write_stream, &mut self.state.shell, focused) {
            Ok(Some(committed)) => {
                if let Some(previous) = committed.previous {
                    clear_endpoint_host_effects(&mut self.state)?;
                    let cancelled = self.endpoint_commands.retire_lane(&previous);
                    if cancel_endpoint_commands(&mut self.state.shell, cancelled).is_needed() {
                        self.state.mark_chrome_dirty();
                    }
                }
                let cancelled =
                    self.endpoint_commands
                        .send_next(&committed.shown, &mut self.write_stream, now);
                if cancel_endpoint_commands(&mut self.state.shell, cancelled).is_needed() {
                    self.state.mark_chrome_dirty();
                }
                self.state.request_repaint();
                self.state.mark_pane_dirty();
            }
            Err(reason) => {
                self.fail_move(shell::EndpointNoticeKind::MoveRejected(reason));
            }
            Ok(None) => {}
        }
        view::release_unwanted(
            &self.state.shell.endpoints.choice,
            &mut self.write_stream,
            &self.state.shell,
            &mut self.next_view_serial,
        );
        self.state.present_pending();
        Ok(())
    }

    /// Fails the move being prepared and reports `notice` against its target. The target
    /// needs no cleanup: it is no longer wanted, so the release step of this same turn turns
    /// it off.
    fn fail_move(&mut self, notice: shell::EndpointNoticeKind) {
        if let Some(failed) = self.state.shell.endpoints.choice.fail_move() {
            present_notice(
                &mut self.state,
                &shell::EndpointNotice::new(failed.to, notice),
            );
        }
    }

    /// Handles one lost connection: supervisor and diagnostic, the choice, its command lane,
    /// the shell status, then the notice for what the loss meant to the choice.
    fn endpoint_lost(
        &mut self,
        failure: &endpoint::EndpointTransportFailure,
        now: std::time::Instant,
    ) -> Result<(), ClientError> {
        let id = &failure.endpoint_id;
        let notice = failure.failure.disconnect_notice();
        let diagnostic = failure.failure.diagnostic();
        let status = endpoint::EndpointFailureStatus::after_failure(&failure.failure);
        self.supervisors
            .record_status(id, failure.generation, status.into(), now);
        self.state.shell.set_machine_diagnostic(id, &diagnostic);
        let lost = self.state.shell.transition_endpoint_status(id, status);
        let cancelled = self.endpoint_commands.disconnect(id);
        let cancellation_repaint = cancel_endpoint_commands(&mut self.state.shell, cancelled);
        if cancellation_repaint.is_needed() {
            self.state.mark_chrome_dirty();
        }
        // The disconnect predicate is fixed UI text; remote diagnostics stay in machine
        // diagnostics, and every raw transport error stays in the log.
        match lost {
            Lost::Shown => {
                present_notice(
                    &mut self.state,
                    &shell::EndpointNotice::new(
                        id.clone(),
                        shell::EndpointNoticeKind::ConnectionLost(notice),
                    ),
                );
                clear_endpoint_host_effects(&mut self.state)?;
            }
            Lost::Target => {
                present_notice(
                    &mut self.state,
                    &shell::EndpointNotice::new(
                        id.clone(),
                        shell::EndpointNoticeKind::MoveInterrupted(notice),
                    ),
                );
            }
            Lost::Unrelated => {
                self.state.mark_chrome_dirty();
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
        let buildbox = endpoint::ClientEndpointId::Ssh(
            shepr_config::MachineLabel::parse("buildbox").expect("machine label"),
        );
        assert_eq!(
            shell::EndpointNotice::new(
                buildbox,
                shell::EndpointNoticeKind::MoveInterrupted("connection was lost; reconnecting"),
            )
            .body(),
            "machine switch interrupted: buildbox connection was lost; reconnecting"
        );
    }
}
