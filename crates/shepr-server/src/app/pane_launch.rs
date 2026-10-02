//! How the app takes a pane launch's settlement (see
//! `shepr_mux::pane::LaunchSettlement`). A runtime exists from the fork, but
//! its shell only from the settlement: a deferred agent resume types its
//! command then, and a launch that failed leaves the pane as a placeholder that
//! says why, instead of removing it.

use bytes::Bytes;
use shepr_core::layout::PaneId;
use shepr_mux::pane::LaunchSettlement;

use super::App;

impl App {
    /// Holds a resume command until its shell launched. The terminal's plan
    /// stays until then, so a save keeps the agent identity, and a launch that
    /// fails still abandons the plan with the reason.
    pub(super) fn hold_resume_command(
        &mut self,
        terminal_id: shepr_protocol::TerminalId,
        command: Bytes,
    ) {
        self.pending_resume_commands.insert(terminal_id, command);
    }

    pub(super) fn handle_pane_launch_settled(
        &mut self,
        pane_id: PaneId,
        settlement: LaunchSettlement,
    ) -> bool {
        let Some(terminal_id) = self
            .find_pane(pane_id)
            .map(|(_, pane)| pane.attached_terminal_id.clone())
        else {
            return false;
        };
        let resume_command = self.pending_resume_commands.remove(&terminal_id);
        match settlement {
            LaunchSettlement::Launched { cwd } => {
                if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                    terminal.set_cwd(cwd);
                }
                if let Some(command) = resume_command {
                    self.send_resume_command(pane_id, &terminal_id, command);
                }
                self.state.mark_session_dirty();
                self.request_git_identity_refresh(self.clock.now);
                true
            }
            LaunchSettlement::Failed(failure) => {
                // The child already exited; its death is reported by the
                // runtime removed here, so it is not reported again.
                self.terminal_runtimes.remove(&terminal_id);
                if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                    if resume_command.is_some() || terminal.pending_agent_resume_plan.is_some() {
                        terminal.abandon_agent_resume(failure, self.clock.now);
                    } else {
                        terminal.restore_error = Some(failure);
                    }
                }
                self.state.mark_session_dirty();
                self.state.mark_shell_projection_dirty();
                self.render_dirty.request_generic();
                self.render_notify.notify_one();
                true
            }
            // The pane's death follows and is handled as any other.
            LaunchSettlement::Unconfirmed => false,
        }
    }

    fn send_resume_command(
        &mut self,
        pane_id: PaneId,
        terminal_id: &shepr_protocol::TerminalId,
        command: Bytes,
    ) {
        let sent = self
            .terminal_runtimes
            .get(terminal_id)
            .map(|runtime| runtime.try_send_bytes(command));
        let Some(terminal) = self.state.terminals.get_mut(terminal_id) else {
            return;
        };
        match sent {
            Some(Ok(())) => terminal.pending_agent_resume_plan = None,
            Some(Err(error)) => {
                tracing::warn!(
                    pane = pane_id.raw(),
                    terminal = %terminal_id,
                    %error,
                    "failed to send deferred agent resume command to shell"
                );
                terminal.abandon_agent_resume(
                    shepr_mux::terminal::RestoreFailure::resume_unavailable(
                        "the resume command could not be sent to the shell",
                    ),
                    self.clock.now,
                );
            }
            None => {}
        }
    }
}
