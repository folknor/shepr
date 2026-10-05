//! Helpers shared within the app module tree. Their items stay parent-scoped,
//! so the crate-visible module declaration exposes no helper API to other
//! crate modules.

/// Hook reports cannot send `unknown`; it remains a distinct detector state in
/// `AgentState`, so the API vocabulary needs this explicit conversion.
pub(super) fn detect_state_from_api(
    state: shepr_api::schema::PaneReportAgentState,
) -> shepr_agent::AgentState {
    match state {
        shepr_api::schema::PaneReportAgentState::Working => shepr_agent::AgentState::Working,
        shepr_api::schema::PaneReportAgentState::Blocked => shepr_agent::AgentState::Blocked,
        shepr_api::schema::PaneReportAgentState::Idle => shepr_agent::AgentState::Idle,
    }
}

pub(super) fn pane_agent_status(state: shepr_agent::AgentState) -> shepr_api::schema::AgentStatus {
    state.presentation_state()
}

/// A user-given pane label as the server stores it: trimmed, and `None` when
/// absent or empty once trimmed, which clears the pane's custom label. Workspace
/// names take the same rule as a `Label` directly, so neither store ever holds
/// an empty or padded name.
pub(super) fn normalized_user_label(label: Option<String>) -> Option<String> {
    shepr_mux::terminal::Label::new(label?).map(shepr_mux::terminal::Label::into_string)
}

#[cfg(test)]
mod agent_status_tests {
    use super::pane_agent_status;
    use shepr_agent::AgentState;
    use shepr_api::schema::AgentStatus;

    #[test]
    fn unknown_agent_state_presents_as_idle() {
        assert_eq!(pane_agent_status(AgentState::Unknown), AgentStatus::Idle);
    }
}
