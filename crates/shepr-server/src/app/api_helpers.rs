use shepr_api::error::ApiError;

pub(crate) fn pane_not_found(pane_id: &str) -> ApiError {
    ApiError::pane_not_found(pane_id)
}

pub(super) fn detect_state_from_api(
    state: shepr_api::schema::PaneAgentState,
) -> shepr_agent::detect::AgentState {
    match state {
        shepr_api::schema::PaneAgentState::Idle => shepr_agent::detect::AgentState::Idle,
        shepr_api::schema::PaneAgentState::Working => shepr_agent::detect::AgentState::Working,
        shepr_api::schema::PaneAgentState::Blocked => shepr_agent::detect::AgentState::Blocked,
        shepr_api::schema::PaneAgentState::Unknown => shepr_agent::detect::AgentState::Unknown,
    }
}

pub(super) fn pane_agent_status(
    state: shepr_agent::detect::AgentState,
) -> shepr_api::schema::AgentStatus {
    match state.presentation_state() {
        shepr_agent::detect::PresentedAgentState::Idle => shepr_api::schema::AgentStatus::Idle,
        shepr_agent::detect::PresentedAgentState::Working => {
            shepr_api::schema::AgentStatus::Working
        }
        shepr_agent::detect::PresentedAgentState::Blocked => {
            shepr_api::schema::AgentStatus::Blocked
        }
    }
}

pub(super) fn normalize_reported_agent_label(agent: &str) -> Option<String> {
    let trimmed = agent.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(agent) = shepr_agent::detect::parse_agent_label(trimmed) {
        return Some(shepr_agent::detect::agent_label(agent).to_string());
    }
    Some(trimmed.to_string())
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
