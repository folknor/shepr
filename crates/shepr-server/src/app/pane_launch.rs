//! How the app takes a pane launch's settlement (see
//! `shepr_mux::pane::LaunchSettlement`). A runtime exists from the fork, but
//! its shell only from the settlement: a deferred agent resume types its
//! command then, and a launch that failed leaves the pane as a placeholder that
//! says why, instead of removing it.

use bytes::Bytes;
use shepr_core::layout::PaneId;
use shepr_mux::pane::{LaunchKind, LaunchOutcome, LaunchSettlement};

use shepr_mux::terminal::TerminalState;

use super::App;

impl App {
    /// Holds a resume command until its shell launched. The terminal's plan
    /// stays until then, so a save keeps the agent identity, and a launch that
    /// fails still abandons the plan with the reason.
    pub(super) fn hold_resume_command(
        &mut self,
        terminal_id: &shepr_protocol::TerminalId,
        command: Bytes,
    ) {
        if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
            terminal.begin_agent_resume_launch(command);
        }
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
        let kind = settlement.kind;
        match settlement.outcome {
            LaunchOutcome::Launched { cwd } => {
                if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                    terminal.set_cwd(cwd);
                }
                let command = self
                    .state
                    .terminals
                    .get_mut(&terminal_id)
                    .filter(|_| kind == LaunchKind::AgentResume)
                    .and_then(TerminalState::take_agent_resume_command);
                if let Some(command) = command {
                    self.send_resume_command(pane_id, &terminal_id, command);
                }
                self.state.mark_session_dirty();
                self.request_git_identity_refresh(self.clock.now);
                true
            }
            LaunchOutcome::Failed(failure) => {
                // The child already exited; its death is reported by the
                // runtime removed here, so it is not reported again.
                self.terminal_runtimes.remove(&terminal_id);
                if kind == LaunchKind::AgentResume {
                    self.abandon_terminal_agent_resume(&terminal_id, failure, self.clock.now);
                } else if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                    terminal.record_start_failure(failure);
                }
                self.state.mark_session_dirty();
                self.state.mark_shell_projection_dirty();
                self.render_dirty.request_generic();
                self.render_notify.notify_one();
                true
            }
            // The pane's death follows and is handled as any other. The
            // runtime stays: the child may still be alive, and its teardown
            // and death report go through the runtime.
            //
            // An agent resume whose launch is unconfirmed is abandoned, not
            // planned again. Its command was never typed (that waits for
            // `Launched`), so this attempt started no agent, but whether its
            // shell child started, or is still stuck in its chdir, is
            // unknown. Returning the plan to `Planned` would retry in a pane
            // whose earlier child may still be alive, and whether the retry
            // can ever run depends on the death that follows removing the
            // runtime, which a held or skipped exit (a checkpointed exit, a
            // signal quit) does not do on this event's schedule. A resume is
            // attempted at most once per restore, so one session is never
            // launched twice. Leaving it `Launching` instead would keep the
            // resume pending with no way to finish. The saved identity is
            // untouched, so the next restore still resumes the session.
            LaunchOutcome::Unconfirmed => {
                if kind != LaunchKind::AgentResume {
                    return false;
                }
                self.abandon_terminal_agent_resume(
                    &terminal_id,
                    shepr_mux::terminal::RestoreFailure::resume_unavailable(
                        "the shell for the resume did not confirm that it started",
                    ),
                    self.clock.now,
                );
                self.state.mark_session_dirty();
                self.state.mark_shell_projection_dirty();
                self.render_dirty.request_generic();
                self.render_notify.notify_one();
                true
            }
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
        match sent {
            Some(Ok(())) => {
                if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
                    terminal.clear_agent_resume();
                }
            }
            Some(Err(error)) => {
                tracing::warn!(
                    pane = pane_id.raw(),
                    terminal = %terminal_id,
                    %error,
                    "failed to send deferred agent resume command to shell"
                );
                self.abandon_terminal_agent_resume(
                    terminal_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::WorkspaceFixture as _;
    use shepr_mux::events::AppEvent;

    /// An app with one pane whose terminal is launching an agent resume
    /// from a live (test) runtime.
    fn app_with_launching_resume() -> (App, PaneId, shepr_protocol::TerminalId) {
        let mut app = App::new(
            &shepr_config::ServerConfig::default(),
            crate::app::AppPolicy::Test,
        );
        let workspace = shepr_mux::workspace::Workspace::test_new("unconfirmed-resume");
        let pane_id = workspace.root_pane();
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("terminal")
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).expect("terminal");
        terminal.plan_agent_resume(crate::test_support::test_codex_plan(
            "unconfirmed",
            vec!["codex".into()],
        ));
        terminal.begin_agent_resume_launch(Bytes::from_static(b"codex resume\r"));
        app.insert_idle_test_runtime(pane_id);
        (app, pane_id, terminal_id)
    }

    fn settle(app: &mut App, pane_id: PaneId, kind: LaunchKind) -> bool {
        let settled = app.from_pane_runtime(
            pane_id,
            AppEvent::PaneLaunchSettled {
                pane_id,
                settlement: LaunchSettlement {
                    kind,
                    outcome: LaunchOutcome::Unconfirmed,
                },
            },
        );
        app.handle_internal_event_with_view_change(settled)
    }

    #[test]
    fn an_unconfirmed_resume_launch_is_abandoned_not_retried() {
        let (mut app, pane_id, terminal_id) = app_with_launching_resume();

        assert!(settle(&mut app, pane_id, LaunchKind::AgentResume));

        let terminal = &app.state.terminals[&terminal_id];
        assert!(
            !terminal.agent_resume().is_pending(),
            "nothing left to retry"
        );
        assert!(matches!(
            terminal.restore_error(),
            Some(shepr_mux::terminal::RestoreFailure::ResumeUnavailable { .. })
        ));
        assert!(!app.has_pending_agent_resumes());
        // The child may still be alive: its runtime stays for the death that
        // follows.
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());
    }

    #[test]
    fn an_unconfirmed_fresh_launch_waits_for_the_death_that_follows() {
        let (mut app, pane_id, terminal_id) = app_with_launching_resume();

        assert!(!settle(&mut app, pane_id, LaunchKind::Fresh));

        let terminal = &app.state.terminals[&terminal_id];
        assert!(terminal.agent_resume().is_launching());
        assert!(terminal.restore_error().is_none());
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());
    }
}
