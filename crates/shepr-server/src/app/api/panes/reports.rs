use super::*;

impl App {
    pub(crate) fn handle_pane_report_agent(
        &mut self,
        params: PaneReportAgentParams,
    ) -> shepr_api::error::ApiResult {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        let Some(agent_label) = normalize_reported_agent_label(&params.agent) else {
            return invalid_agent();
        };
        self.handle_internal_event(shepr_mux::events::AppEvent::HookStateReported {
            pane_id,
            session_ref: shepr_agent::agent::resume::session_ref_from_report(
                &params.source,
                &agent_label,
                params.agent_session_id,
                params.agent_session_path,
            ),
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
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        let Some(agent_label) = normalize_reported_agent_label(&params.agent) else {
            return invalid_agent();
        };
        self.handle_internal_event(shepr_mux::events::AppEvent::AgentSessionReported {
            pane_id,
            session_ref: shepr_agent::agent::resume::session_ref_from_report(
                &params.source,
                &agent_label,
                params.agent_session_id,
                params.agent_session_path,
            ),
            source: params.source,
            agent_label,
            seq: params.seq,
            session_start_source: shepr_agent::agent::resume::normalize_session_start_source(
                params.session_start_source.as_deref(),
            ),
        });

        success(ResponseResult::Ok {})
    }

    pub(crate) fn handle_pane_clear_agent_authority(
        &mut self,
        params: PaneClearAgentAuthorityParams,
    ) -> shepr_api::error::ApiResult {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        self.handle_internal_event(shepr_mux::events::AppEvent::HookAuthorityCleared {
            pane_id,
            source: params.source,
            seq: params.seq,
        });

        success(ResponseResult::Ok {})
    }
}
