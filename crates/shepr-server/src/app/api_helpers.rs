//! Helpers shared within the app module tree. Their items stay parent-scoped,
//! so the crate-visible module declaration exposes no helper API to other
//! crate modules.

use shepr_api::error::ApiError;

pub(super) fn pane_not_found(pane_id: &str) -> ApiError {
    ApiError::pane_not_found(pane_id)
}

pub(super) fn detect_state_from_api(
    state: shepr_api::schema::PaneAgentState,
) -> shepr_agent::AgentState {
    state
}

pub(super) fn pane_agent_status(state: shepr_agent::AgentState) -> shepr_api::schema::AgentStatus {
    presented_agent_status(state.presentation_state())
}

pub(super) fn presented_agent_status(
    state: shepr_agent::PresentedAgentState,
) -> shepr_api::schema::AgentStatus {
    state
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

#[cfg(test)]
mod not_found_tests {
    use super::pane_not_found;
    use shepr_api::error::ApiErrorCode;

    #[test]
    fn pane_not_found_keeps_the_subject_and_code_together() {
        let error = pane_not_found("w1:p2");
        assert_eq!(error.code, ApiErrorCode::PaneNotFound);
        assert_eq!(error.into_message(), "pane w1:p2 not found");
    }
}
