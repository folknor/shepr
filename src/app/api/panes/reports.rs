use super::*;

impl App {
    pub(crate) fn handle_pane_report_agent(
        &mut self,
        id: String,
        params: PaneReportAgentParams,
    ) -> crate::api::error::ApiResult {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(agent_label) = normalize_reported_agent_label(&params.agent) else {
            return invalid_agent(id);
        };
        self.handle_internal_event(crate::events::AppEvent::HookStateReported {
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

        success(id, ResponseResult::Ok {})
    }

    pub(crate) fn handle_pane_report_agent_session(
        &mut self,
        id: String,
        params: PaneReportAgentSessionParams,
    ) -> crate::api::error::ApiResult {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(agent_label) = normalize_reported_agent_label(&params.agent) else {
            return invalid_agent(id);
        };
        self.handle_internal_event(crate::events::AppEvent::AgentSessionReported {
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

        success(id, ResponseResult::Ok {})
    }

    pub(crate) fn handle_pane_report_metadata(
        &mut self,
        id: String,
        params: PaneReportMetadataParams,
    ) -> crate::api::error::ApiResult {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let agent_label = match params.agent.as_deref() {
            Some(agent) => match normalize_reported_agent_label(agent) {
                Some(agent_label) => Some(agent_label),
                None => return invalid_agent(id),
            },
            None => None,
        };
        let source = match normalize_metadata_source(&params.source) {
            Ok(source) => source,
            Err(message) => {
                return failure(
                    id,
                    crate::api::error::ApiErrorCode::InvalidMetadataSource,
                    message,
                );
            }
        };
        let raw_title_set = params.title.is_some();
        let raw_display_agent_set = params.display_agent.is_some();
        let raw_state_labels_set = !params.state_labels.is_empty();
        let tokens = if params.tokens.is_empty() {
            None
        } else {
            match normalize_metadata_tokens(params.tokens) {
                Ok(tokens) => Some(tokens),
                Err(message) => {
                    return failure(
                        id,
                        crate::api::error::ApiErrorCode::InvalidMetadataToken,
                        message,
                    );
                }
            }
        };
        let ttl = match normalize_metadata_ttl(params.ttl_ms) {
            Ok(ttl) => ttl,
            Err(message) => {
                return failure(
                    id,
                    crate::api::error::ApiErrorCode::InvalidMetadataTtl,
                    message,
                );
            }
        };
        let title = normalize_presentation_text(params.title);
        let display_agent = normalize_presentation_text(params.display_agent);
        let applies_to_source = match params.applies_to_source {
            Some(ref applies_to_source) => match normalize_metadata_source(applies_to_source) {
                Ok(applies_to_source) => Some(applies_to_source),
                Err(message) => {
                    return failure(
                        id,
                        crate::api::error::ApiErrorCode::InvalidMetadataSource,
                        message,
                    );
                }
            },
            None => None,
        };
        let state_labels = match normalize_state_labels(params.state_labels) {
            Ok(labels) => labels,
            Err(status) => {
                return failure(
                    id,
                    crate::api::error::ApiErrorCode::InvalidStateLabel,
                    format!("unknown state label: {status}"),
                );
            }
        };
        if raw_title_set && params.clear_title
            || raw_display_agent_set && params.clear_display_agent
            || raw_state_labels_set && params.clear_state_labels
        {
            return failure(
                id,
                crate::api::error::ApiErrorCode::InvalidMetadataRequest,
                "cannot set and clear the same metadata field",
            );
        }
        if title.is_none()
            && display_agent.is_none()
            && state_labels.is_empty()
            && tokens.is_none()
            && !params.clear_title
            && !params.clear_display_agent
            && !params.clear_state_labels
        {
            return failure(
                id,
                crate::api::error::ApiErrorCode::InvalidMetadataRequest,
                "missing metadata field to set or clear",
            );
        }
        let presentation_requested = title.is_some()
            || display_agent.is_some()
            || !state_labels.is_empty()
            || params.clear_title
            || params.clear_display_agent
            || params.clear_state_labels;
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.pane_state(pane_id))
            .map(|pane| pane.attached_terminal_id.clone())
        else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        if terminal.metadata_report_blocked_by_process_exit(
            &source,
            agent_label.as_deref(),
            applies_to_source.as_deref(),
        ) {
            return success(id, ResponseResult::Ok {});
        }
        if !terminal.metadata_report_sequence_is_fresh(&source, params.seq) {
            return success(id, ResponseResult::Ok {});
        }
        let metadata_agent = crate::terminal::TerminalState::metadata_report_agent(
            &source,
            agent_label.as_deref(),
            applies_to_source.as_deref(),
        );
        if let Some(tokens) = tokens.as_ref()
            && terminal.metadata_tokens.key_count_after_patch(tokens)
                > MAX_METADATA_TOKEN_KEYS_PER_RESOURCE
        {
            return failure(
                id,
                crate::api::error::ApiErrorCode::MetadataTokenLimit,
                format!(
                    "pane metadata may contain at most {MAX_METADATA_TOKEN_KEYS_PER_RESOURCE} tokens"
                ),
            );
        }
        match terminal.accept_metadata_report(&source, params.seq, tokens.is_some(), metadata_agent)
        {
            Ok(true) => {}
            Ok(false) => return success(id, ResponseResult::Ok {}),
            Err(()) => {
                return failure(
                    id,
                    crate::api::error::ApiErrorCode::MetadataSequenceSourceLimit,
                    format!(
                        "pane metadata may track at most {} sequenced sources",
                        crate::terminal::metadata_tokens::MAX_SEQUENCE_SOURCES
                    ),
                );
            }
        }
        let token_changed = tokens.is_some_and(|tokens| {
            let changed = terminal
                .metadata_tokens
                .patch(tokens, ttl, std::time::Instant::now());
            if changed {
                terminal.revision = terminal.revision.saturating_add(1);
            }
            changed
        });

        if presentation_requested {
            self.handle_internal_event(crate::events::AppEvent::HookMetadataReported {
                pane_id,
                source,
                agent_label,
                applies_to_source,
                title,
                display_agent,
                state_labels,
                clear_title: params.clear_title,
                clear_display_agent: params.clear_display_agent,
                clear_state_labels: params.clear_state_labels,
                seq: None,
                ttl,
            });
        }
        if token_changed {
            self.sync_agent_metadata_deadline();
            self.emit_pane_updated(ws_idx, pane_id);
        }

        success(id, ResponseResult::Ok {})
    }

    pub(crate) fn handle_pane_clear_agent_authority(
        &mut self,
        id: String,
        params: PaneClearAgentAuthorityParams,
    ) -> crate::api::error::ApiResult {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        self.handle_internal_event(crate::events::AppEvent::HookAuthorityCleared {
            pane_id,
            source: params.source,
            seq: params.seq,
        });

        success(id, ResponseResult::Ok {})
    }

    pub(crate) fn handle_pane_release_agent(
        &mut self,
        id: String,
        params: PaneReleaseAgentParams,
    ) -> crate::api::error::ApiResult {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(agent_label) = normalize_reported_agent_label(&params.agent) else {
            return invalid_agent(id);
        };
        self.handle_internal_event(crate::events::AppEvent::HookAgentReleased {
            pane_id,
            source: params.source,
            known_agent: shepr_agent::detect::parse_agent_label(&agent_label),
            agent_label,
            seq: params.seq,
        });

        success(id, ResponseResult::Ok {})
    }
}
