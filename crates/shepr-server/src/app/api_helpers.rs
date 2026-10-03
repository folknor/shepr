//! Helpers shared within the app module tree. Their items stay parent-scoped,
//! so the crate-visible module declaration exposes no helper API to other
//! crate modules.

use shepr_api::error::ApiError;

pub(super) fn pane_not_found(pane_id: &str) -> ApiError {
    ApiError::pane_not_found(pane_id)
}

pub(super) fn detect_state_from_api(
    state: shepr_api::schema::PaneAgentState,
) -> shepr_agent::detect::AgentState {
    state
}

pub(super) fn pane_agent_status(
    state: shepr_agent::detect::AgentState,
) -> shepr_api::schema::AgentStatus {
    presented_agent_status(state.presentation_state())
}

pub(super) fn presented_agent_status(
    state: shepr_agent::detect::PresentedAgentState,
) -> shepr_api::schema::AgentStatus {
    state
}

/// A user-given workspace or pane label as the server stores it: trimmed, and
/// one that is absent or empty once trimmed clears the custom name, so the
/// automatic label returns. Every rename and create path goes through this, so
/// no store ever holds an empty or padded label.
pub(super) fn normalized_user_label(label: Option<String>) -> Option<String> {
    let label = label?;
    let trimmed = label.trim();
    if trimmed.is_empty() {
        None
    } else if trimmed.len() == label.len() {
        Some(label)
    } else {
        Some(trimmed.to_owned())
    }
}

#[cfg(test)]
mod agent_status_tests {
    use super::pane_agent_status;
    use shepr_agent::detect::AgentState;
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
