use super::*;

impl TerminalState {
    pub fn with_pending_agent_resume_plan(
        mut self,
        plan: shepr_agent::resume::AgentResumePlan,
    ) -> Self {
        self.agent_resume = AgentResumeState::Planned(plan);
        self
    }

    /// The deferred resume cannot continue, and the pane is left without a
    /// runtime: none was ever started, or the caller retires the one it had.
    /// Ends the resume, records why (with the plan, when there was one, so
    /// the failure can name the agent, session and command for a manual
    /// resume), and withdraws the detection
    /// `restored_terminal` seeded for the resumed agent: with no detector to
    /// ever report the pane empty, that seed would show an idle agent on a
    /// dead pane indefinitely. The saved session identity stays in ownership,
    /// as for any unavailable restored pane, so a later save writes it back.
    // Failure presentation and persistence are handled by the resume caller.
    // Removing its synthetic detected identity is not a new agent activity
    // transition: Unknown and Idle have the same sidebar presentation.
    pub fn abandon_agent_resume(
        &mut self,
        error: super::PaneStartFailure,
        now: Instant,
    ) -> shepr_detect::ownership::AgentOwnershipMutation {
        let error = match self.agent_resume.plan() {
            Some(plan) => super::PaneStartFailure::ResumeFailed {
                plan: plan.clone(),
                failure: Box::new(error),
            },
            None => error,
        };
        self.agent_resume = AgentResumeState::None;
        self.start_failure = Some(error);
        if self.ownership.detected_agent().is_some() {
            return self.ownership.set_detected_state_with_screen_signals_at(
                None,
                AgentState::Unknown,
                false,
                false,
                now,
            );
        }
        shepr_detect::ownership::AgentOwnershipMutation::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandoning_a_launch_keeps_the_plan_for_manual_recovery() {
        let session = shepr_agent::resume::PersistedAgentSession::new(
            shepr_agent::AgentSource::parse("shepr:codex").expect("bundled test source"),
            shepr_agent::resume::AgentSessionRef::id("session; literal").expect("valid session id"),
        )
        .expect("valid Codex session");
        let plan = session.resume_plan();
        let mut terminal = TerminalState::new(
            shepr_core::absolute_path::AbsolutePath::new("/nonexistent/resume-cwd")
                .expect("absolute cwd"),
        )
        .with_pending_agent_resume_plan(plan.clone());
        terminal.begin_agent_resume_launch(bytes::Bytes::from("command\r"));
        terminal.take_agent_resume_command();
        terminal.abandon_agent_resume(
            PaneStartFailure::resume_unavailable(ResumeUnavailableReason::CommandSendFailed),
            Instant::now(),
        );
        assert!(!terminal.agent_resume().is_pending());
        let failure = terminal.start_failure().expect("failure placeholder");
        let PaneStartFailure::ResumeFailed { plan: kept, .. } = failure else {
            panic!("resume failure must retain its plan");
        };
        assert_eq!(kept, &plan);
        let text = failure.to_string();
        assert!(text.contains("Agent: codex"));
        assert!(text.contains("Session: session; literal"));
        assert!(text.contains("Manual resume: codex resume 'session; literal'"));
        assert!(text.contains(ResumeUnavailableReason::CommandSendFailed.as_str()));
    }

    #[test]
    fn refused_shell_keeps_the_resume_plan_and_original_errno() {
        let session = shepr_agent::resume::PersistedAgentSession::new(
            shepr_agent::AgentSource::parse("shepr:codex").expect("bundled test source"),
            shepr_agent::resume::AgentSessionRef::id("cold-session").expect("valid id"),
        )
        .expect("valid Codex session");
        let plan = session.resume_plan();
        let mut terminal = TerminalState::new(
            shepr_core::absolute_path::AbsolutePath::new("/nonexistent/resume-cwd")
                .expect("absolute cwd"),
        )
        .with_pending_agent_resume_plan(plan.clone());
        terminal.abandon_agent_resume(
            PaneStartFailure::shell_start_failed(&std::io::Error::from_raw_os_error(libc::ENOENT)),
            Instant::now(),
        );
        let Some(PaneStartFailure::ResumeFailed {
            plan: kept,
            failure,
        }) = terminal.start_failure()
        else {
            panic!("refused shell must retain its resume plan");
        };
        assert_eq!(kept, &plan);
        let PaneStartFailure::ShellStartFailed { error, .. } = failure.as_ref() else {
            panic!("original shell failure must remain available");
        };
        assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
    }
}
