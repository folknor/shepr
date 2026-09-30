use super::*;

impl App {
    pub(crate) fn handle_pane_report_agent(
        &mut self,
        params: PaneReportAgentParams,
    ) -> shepr_api::error::ApiResult {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return Err(pane_not_found(&params.pane_id));
        };
        let Some(agent_label) = normalize_reported_agent_label(&params.agent) else {
            return invalid_agent();
        };
        let session_ref = parse_report_session_ref(
            &params.source,
            &agent_label,
            params.agent_session_id,
            params.agent_session_path,
        )?;
        self.handle_internal_event(shepr_mux::events::AppEvent::HookStateReported {
            pane_id,
            session_ref,
            source: params.source,
            agent_label,
            state: detect_state_from_api(params.state),
            message: params.message,
            seq: params.seq,
        });

        success(ResponseResult::Ok {})
    }

    pub(crate) fn handle_pane_report_agent_session(
        &mut self,
        params: PaneReportAgentSessionParams,
    ) -> shepr_api::error::ApiResult {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return Err(pane_not_found(&params.pane_id));
        };
        let Some(agent_label) = normalize_reported_agent_label(&params.agent) else {
            return invalid_agent();
        };
        let session_ref = parse_report_session_ref(
            &params.source,
            &agent_label,
            params.agent_session_id,
            params.agent_session_path,
        )?;
        self.handle_internal_event(shepr_mux::events::AppEvent::AgentSessionReported {
            pane_id,
            session_ref,
            source: params.source,
            agent_label,
            seq: params.seq,
            session_start_source: shepr_agent::agent::resume::normalize_session_start_source(
                params.session_start_source.as_deref(),
            ),
        });

        success(ResponseResult::Ok {})
    }
}

/// Source parsing and reference validation happen before dispatch. An absent
/// reference is a state-only report; a supplied invalid official reference is
/// a bad request and must not be mistaken for that absence. Custom reports do
/// not own resume identities. Internal state events still carry separate source
/// and label strings, so TerminalState also validates non-API callers.
fn parse_report_session_ref(
    source: &str,
    agent_label: &str,
    id: Option<String>,
    path: Option<String>,
) -> Result<Option<shepr_agent::agent::resume::AgentSessionRef>, shepr_api::error::ApiError> {
    let Some(agent) = shepr_agent::agent::Agent::parse_source(source) else {
        return Ok(None);
    };
    if agent.label() != agent_label {
        return failure(
            ApiErrorCode::InvalidAgent,
            "report source does not match agent label",
        );
    }
    let supplied = id.is_some() || path.is_some();
    let session_ref = shepr_agent::agent::resume::session_ref_for_agent_report(agent, id, path);
    if supplied && session_ref.is_none() {
        return failure(
            ApiErrorCode::InvalidRequest,
            "invalid agent session reference",
        );
    }
    Ok(session_ref)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_session_ref_is_distinct_from_invalid_supplied_ref() {
        assert!(
            parse_report_session_ref("shepr:kimi", "kimi", None, None)
                .expect("state-only report")
                .is_none()
        );
        assert!(parse_report_session_ref("shepr:kimi", "kimi", Some(String::new()), None).is_err());
        assert!(
            parse_report_session_ref("shepr:kimi", "kimi", None, Some("/session.jsonl".into()))
                .is_err()
        );
    }

    #[test]
    fn official_source_cannot_claim_another_agent() {
        assert!(
            parse_report_session_ref("shepr:kimi", "kilo", Some("session".into()), None).is_err()
        );
        assert!(
            parse_report_session_ref("shepr:kimi", "kimi", Some("session".into()), None)
                .expect("official identity")
                .is_some()
        );
    }

    #[test]
    fn custom_state_report_does_not_mint_a_resume_identity() {
        assert!(
            parse_report_session_ref("custom:state", "kimi", Some("session".into()), None)
                .expect("custom state report")
                .is_none()
        );
    }
}
