//! How the app takes a pane launch's settlement (see
//! `shepr_mux::pane::LaunchSettlement`). A runtime exists from the fork, but
//! its shell only from the settlement: a deferred agent resume types its
//! command then, and a launch that failed leaves the pane as a placeholder that
//! says why, instead of removing it.

use bytes::Bytes;
use shepr_core::layout::PaneId;
use shepr_mux::pane::{LaunchKind, LaunchOutcome, LaunchSettlement};

use shepr_mux::terminal::{PaneStartFailure, ResumeUnavailableReason, TerminalState};
use shepr_mux::workspace::PaneRecord;

use super::App;

impl App {
    /// Holds a resume command until its shell launched. The terminal's plan
    /// stays until then, so a save keeps the agent identity, and a launch that
    /// fails still abandons the plan with the reason.
    pub(super) fn hold_resume_command(&mut self, pane_id: PaneId, command: Bytes) {
        if let Some(record) = self.state.workspaces.pane_mut(pane_id) {
            record.terminal_mut().begin_agent_resume_launch(command);
        }
    }

    pub(super) fn handle_pane_launch_settled(
        &mut self,
        pane_id: PaneId,
        settlement: LaunchSettlement,
    ) -> bool {
        if self.state.terminal(pane_id).is_none() {
            return false;
        }
        let kind = settlement.kind;
        match settlement.outcome {
            LaunchOutcome::Launched { cwd } => {
                if let Some(record) = self.state.workspaces.pane_mut(pane_id) {
                    record.terminal_mut().set_cwd(cwd);
                }
                let command = self
                    .state
                    .workspaces
                    .pane_mut(pane_id)
                    .map(PaneRecord::terminal_mut)
                    .filter(|_| kind == LaunchKind::AgentResume)
                    .and_then(TerminalState::take_agent_resume_command);
                if kind == LaunchKind::AgentResume {
                    if let Some(command) = command {
                        self.send_resume_command(pane_id, command);
                    } else {
                        tracing::error!(
                            pane = %pane_id,
                            "agent resume launch settled without a pending command"
                        );
                        self.fail_agent_resume(pane_id, ResumeUnavailableReason::CommandSendFailed);
                    }
                }
                self.state.mark_session_dirty();
                self.request_git_identity_refresh(self.clock.now);
                true
            }
            LaunchOutcome::Failed(failure) => {
                // The child already exited; its death is reported by the
                // runtime removed here, so it is not reported again.
                self.terminal_runtimes.remove(&pane_id);
                if kind == LaunchKind::AgentResume {
                    self.abandon_agent_resume(pane_id, failure, self.clock.now);
                } else if let Some(record) = self.state.workspaces.pane_mut(pane_id) {
                    record.terminal_mut().record_start_failure(failure);
                }
                self.state.mark_session_dirty();
                self.state.mark_shell_projection_dirty();
                true
            }
            // The child is gone, or the pane ended before the launch settled,
            // and the pane's death follows as an ordinary one. An agent resume
            // is the exception: an ordinary death would remove the pane and
            // its saved session with it, so the resume fails into a
            // placeholder that keeps the session (`fail_agent_resume`). The
            // resume command was never typed, since that waits for `Launched`.
            LaunchOutcome::Unconfirmed => {
                if kind != LaunchKind::AgentResume {
                    return false;
                }
                self.fail_agent_resume(pane_id, ResumeUnavailableReason::ShellLaunchUnconfirmed);
                true
            }
            // The child may be alive, but with its status unreadable it never
            // opens observation: no liveness, no detection, no exit to wait
            // for. Retiring the runtime ends it, and the pane stays as a
            // placeholder saying why, as for a launch that failed. The
            // coordinator already logged the error.
            LaunchOutcome::StatusUnavailable(error) => {
                if kind == LaunchKind::AgentResume {
                    self.fail_agent_resume(
                        pane_id,
                        ResumeUnavailableReason::ShellLaunchUnconfirmed,
                    );
                    return true;
                }
                self.terminal_runtimes.remove(&pane_id);
                if let Some(record) = self.state.workspaces.pane_mut(pane_id) {
                    record
                        .terminal_mut()
                        .record_start_failure(PaneStartFailure::launch_unobservable(&error));
                }
                self.state.mark_session_dirty();
                self.state.mark_shell_projection_dirty();
                true
            }
        }
    }

    /// A deferred agent resume that cannot go on ends its shell too: the pane
    /// becomes a placeholder that says why and keeps the saved agent session,
    /// so a save writes it back and the next restore resumes it. The failure
    /// is drawn only on a pane without a runtime; a live shell's rows are its
    /// own, and its PTY patches would repaint any notice laid over them.
    fn fail_agent_resume(&mut self, pane_id: PaneId, reason: ResumeUnavailableReason) {
        let agent = self
            .state
            .terminal(pane_id)
            .and_then(|terminal| terminal.ownership().persisted_agent_session())
            .map(shepr_agent::resume::PersistedAgentSession::agent);
        tracing::warn!(
            pane = %pane_id,
            ?agent,
            reason = reason.as_str(),
            "deferred agent resume failed; keeping the pane as a placeholder with its saved session"
        );
        self.terminal_runtimes.remove(&pane_id);
        self.abandon_agent_resume(
            pane_id,
            PaneStartFailure::resume_unavailable(reason),
            self.clock.now,
        );
        self.state.mark_session_dirty();
        self.state.mark_shell_projection_dirty();
    }

    fn send_resume_command(&mut self, pane_id: PaneId, command: Bytes) {
        let sent = self
            .terminal_runtimes
            .get(&pane_id)
            .map(|runtime| runtime.try_send_bytes(command));
        match sent {
            Some(Ok(())) => {
                if let Some(record) = self.state.workspaces.pane_mut(pane_id) {
                    record.terminal_mut().clear_agent_resume();
                }
            }
            Some(Err(error)) => {
                tracing::warn!(
                    pane = %pane_id,
                    %error,
                    "failed to send deferred agent resume command to shell"
                );
                self.fail_agent_resume(pane_id, ResumeUnavailableReason::CommandSendFailed);
            }
            // Admission of the settlement requires the runtime's current
            // generation, so this is unreachable today; it still fails the
            // resume rather than leave it pending forever.
            None => {
                tracing::error!(
                    pane = %pane_id,
                    "cannot send deferred agent resume command: pane has no live runtime"
                );
                self.fail_agent_resume(pane_id, ResumeUnavailableReason::CommandSendFailed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::WorkspaceFixture as _;
    use shepr_mux::events::RuntimeEvent;

    /// An app with one pane whose terminal is launching an agent resume
    /// from a live (test) runtime.
    fn app_with_launching_resume() -> (crate::app::TestApp, PaneId) {
        let mut app = App::new(&shepr_config::ServerConfig::default());
        let workspace = shepr_mux::workspace::Workspace::test_new("unconfirmed-resume");
        let pane_id = workspace.tree().root();
        app.state.test_set_workspaces(vec![workspace]);
        let terminal = app.state.terminal_mut(pane_id);
        let plan = crate::test_support::test_codex_plan("unconfirmed", vec!["codex".into()]);
        terminal
            .ownership_mut()
            .set_persisted_agent_session(plan.key().clone());
        terminal.plan_agent_resume(plan);
        terminal.begin_agent_resume_launch(Bytes::from_static(b"codex resume\r"));
        app.insert_idle_test_runtime(pane_id);
        (app, pane_id)
    }

    fn settle(app: &mut App, pane_id: PaneId, kind: LaunchKind) -> bool {
        settle_as(app, pane_id, kind, LaunchOutcome::Unconfirmed)
    }

    fn settle_as(app: &mut App, pane_id: PaneId, kind: LaunchKind, outcome: LaunchOutcome) -> bool {
        let settled = app.from_pane_runtime(
            pane_id,
            RuntimeEvent::PaneLaunchSettled {
                settlement: LaunchSettlement { kind, outcome },
            },
        );
        app.handle_internal_event_with_view_change(settled)
    }

    fn status_unavailable() -> LaunchOutcome {
        LaunchOutcome::StatusUnavailable(std::io::Error::other("status channel failed"))
    }

    #[test]
    fn an_unobservable_fresh_launch_is_ended_into_a_placeholder() {
        let (mut app, pane_id) = app_with_launching_resume();

        assert!(settle_as(
            &mut app,
            pane_id,
            LaunchKind::Fresh,
            status_unavailable()
        ));

        let terminal = app.state.terminal(pane_id).expect("the pane stays");
        assert!(matches!(
            terminal.restore_error(),
            Some(shepr_mux::terminal::PaneStartFailure::LaunchUnobservable { .. })
        ));
        assert!(app.terminal_runtimes.get(&pane_id).is_none());
    }

    #[test]
    fn an_unobservable_resume_launch_keeps_the_saved_session_in_a_placeholder() {
        let (mut app, pane_id) = app_with_launching_resume();

        assert!(settle_as(
            &mut app,
            pane_id,
            LaunchKind::AgentResume,
            status_unavailable()
        ));

        let terminal = app.state.terminal(pane_id).expect("the pane stays");
        assert!(matches!(
            terminal.restore_error(),
            Some(shepr_mux::terminal::PaneStartFailure::ResumeUnavailable { .. })
        ));
        assert!(terminal.ownership().persisted_agent_session().is_some());
        assert!(!app.has_pending_agent_resumes());
        assert!(app.terminal_runtimes.get(&pane_id).is_none());
    }

    #[test]
    fn an_unconfirmed_resume_launch_is_abandoned_not_retried() {
        let (mut app, pane_id) = app_with_launching_resume();

        assert!(settle(&mut app, pane_id, LaunchKind::AgentResume));

        let terminal = app.state.terminal(pane_id).expect("terminal");
        assert!(
            !terminal.agent_resume().is_pending(),
            "nothing left to retry"
        );
        assert!(matches!(
            terminal.restore_error(),
            Some(shepr_mux::terminal::PaneStartFailure::ResumeUnavailable { .. })
        ));
        assert!(terminal.ownership().persisted_agent_session().is_some());
        assert!(!app.has_pending_agent_resumes());
        // The unconfirmed launch is retired before its queued exit can remove
        // the pane and its saved session.
        assert!(app.terminal_runtimes.get(&pane_id).is_none());
    }

    #[test]
    fn an_unconfirmed_fresh_launch_waits_for_the_death_that_follows() {
        let (mut app, pane_id) = app_with_launching_resume();

        assert!(!settle(&mut app, pane_id, LaunchKind::Fresh));

        let terminal = app.state.terminal(pane_id).expect("terminal");
        assert!(terminal.agent_resume().is_launching());
        assert!(terminal.restore_error().is_none());
        assert!(app.terminal_runtimes.get(&pane_id).is_some());
    }
}
