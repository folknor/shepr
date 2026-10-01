use super::*;

impl App {
    pub(crate) fn handle_pane_report_agent(
        &mut self,
        params: PaneReportAgentParams,
    ) -> shepr_api::error::ApiResult {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return Err(pane_not_found(&params.pane_id));
        };
        let (source, agent_label, session_ref) = Self::parse_agent_report_identity(
            &params.source,
            &params.agent,
            params.agent_session_id,
            params.agent_session_path,
        )?;
        self.handle_internal_event(shepr_mux::events::AppEvent::HookStateReported {
            pane_id,
            session_ref,
            source,
            agent_label,
            state: detect_state_from_api(params.state),
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
        let (source, agent_label, session_ref) = Self::parse_agent_report_identity(
            &params.source,
            &params.agent,
            params.agent_session_id,
            params.agent_session_path,
        )?;
        self.handle_internal_event(shepr_mux::events::AppEvent::AgentSessionReported {
            pane_id,
            session_ref,
            source,
            agent_label,
            seq: params.seq,
            session_start_source: shepr_agent::agent::resume::normalize_session_start_source(
                params.session_start_source.as_deref(),
            ),
        });

        success(ResponseResult::Ok {})
    }

    /// Parses the identity shared by both report handlers: source, normalized
    /// agent label and validated session reference.
    fn parse_agent_report_identity(
        source_text: &str,
        agent_text: &str,
        id: Option<String>,
        path: Option<String>,
    ) -> Result<
        (
            shepr_agent::agent::AgentSource,
            String,
            Option<shepr_agent::agent::resume::AgentSessionRef>,
        ),
        shepr_api::error::ApiError,
    > {
        let Some(agent_label) = normalize_reported_agent_label(agent_text) else {
            return invalid_agent();
        };
        let source = shepr_agent::agent::AgentSource::parse(source_text);
        let session_ref = parse_report_session_ref(&source, &agent_label, id, path)?;
        Ok((source, agent_label, session_ref))
    }
}

/// Source parsing and reference validation happen before dispatch. An absent
/// reference is a state-only report; a supplied invalid official reference is
/// a bad request and must not be mistaken for that absence. Custom reports do
/// not own resume identities. The parsed source is carried through internal
/// events; TerminalState also validates the label for non-API callers.
fn parse_report_session_ref(
    source: &shepr_agent::agent::AgentSource,
    agent_label: &str,
    id: Option<String>,
    path: Option<String>,
) -> Result<Option<shepr_agent::agent::resume::AgentSessionRef>, shepr_api::error::ApiError> {
    let Some(agent) = source.agent() else {
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
    use std::path::Path;

    use crate::test_support::{IsolatedEnv, ScratchDir};
    use shepr_api::schema::{Method, Request};

    #[test]
    fn missing_session_ref_is_distinct_from_invalid_supplied_ref() {
        assert!(
            parse_report_session_ref(&"shepr:kimi".into(), "kimi", None, None)
                .expect("state-only report")
                .is_none()
        );
        assert!(
            parse_report_session_ref(&"shepr:kimi".into(), "kimi", Some(String::new()), None)
                .is_err()
        );
        assert!(
            parse_report_session_ref(
                &"shepr:kimi".into(),
                "kimi",
                None,
                Some("/session.jsonl".into())
            )
            .is_err()
        );
    }

    #[test]
    fn official_source_cannot_claim_another_agent() {
        assert!(
            parse_report_session_ref(&"shepr:kimi".into(), "kilo", Some("session".into()), None)
                .is_err()
        );
        assert!(
            parse_report_session_ref(&"shepr:kimi".into(), "kimi", Some("session".into()), None)
                .expect("official identity")
                .is_some()
        );
    }

    #[test]
    fn official_report_accepts_a_supported_path_session() {
        let session_ref = parse_report_session_ref(
            &"shepr:pi".into(),
            "pi",
            None,
            Some("/sessions/pi.jsonl".into()),
        )
        .expect("supported path session")
        .expect("report carries a session reference");

        assert_eq!(
            session_ref.kind(),
            shepr_agent::agent::resume::AgentSessionRefKind::Path
        );
        assert_eq!(session_ref.value_str(), "/sessions/pi.jsonl");
    }

    #[test]
    fn custom_state_report_does_not_mint_a_resume_identity() {
        assert!(
            parse_report_session_ref(&"custom:state".into(), "kimi", Some("session".into()), None)
                .expect("custom state report")
                .is_none()
        );
    }

    #[test]
    fn broken_agent_asset_is_rejected_by_the_report_handler() {
        let _environment = IsolatedEnv::new();
        let scratch = ScratchDir::new("agent-report-handler-mutation");
        let asset = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../shepr-agent/src/integration/assets/claude/shepr-agent-state.sh");
        let source = std::fs::read_to_string(&asset).expect("read Claude integration asset");
        let broken = source.replacen("\"agent\": \"claude\"", "\"agent\": \"codex\"", 1);
        assert_ne!(broken, source, "mutation probe must change the asset");
        let broken_path = scratch.join("broken-claude-state.sh");
        std::fs::write(&broken_path, broken).expect("write broken asset copy in scratch");

        let detected = shepr_mux::terminal::state::HookClockSample {
            monotonic: std::time::Instant::now(),
            wall: std::time::SystemTime::now(),
        };
        let mut app = crate::agent_report_test_support::AgentReportHarness::new(
            scratch.path(),
            shepr_agent::agent::Agent::Claude,
            detected,
        )
        .expect("build report handler App");
        let request = capture_broken_asset_request(&broken_path, &scratch, app.pane_id());
        std::fs::remove_file(&broken_path).expect("remove broken asset copy");
        let request: Request = serde_json::from_value(request).expect("parse captured API request");
        assert!(matches!(&request.method, Method::PaneReportAgentSession(_)));

        let error = app
            .apply_request(
                request,
                shepr_mux::terminal::state::HookClockSample {
                    monotonic: detected.monotonic + std::time::Duration::from_millis(1),
                    wall: detected.wall + std::time::Duration::from_millis(1),
                },
            )
            .expect_err("report handler rejects an official source with another agent label");
        assert_eq!(error.code, ApiErrorCode::InvalidAgent);
        assert!(error.into_message().contains("does not match"));
        assert!(
            app.terminal_state()
                .expect("the test pane keeps its terminal")
                .current_session_identity_for_persistence()
                .is_none()
        );
    }

    fn capture_broken_asset_request(
        script: &Path,
        scratch: &ScratchDir,
        pane_id: &str,
    ) -> serde_json::Value {
        let socket_path = scratch.join("report-handler.sock");
        // host-program-ok: the shipped agent hook is the shell script under test.
        let mut command = shepr_test_support::command_in_scratch("sh", "agent-report-handler");
        // Hook assets report only from panes of a release server.
        command
            .arg(script)
            .arg("session")
            .env("SHEPR_BUILD_PROFILE", "release");
        let mut input = serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": "claude-contract-session",
            "source": "startup",
        })
        .to_string()
        .into_bytes();
        input.push(b'\n');
        let output = shepr_test_support::capture_hook(
            command,
            &socket_path,
            scratch.path(),
            pane_id,
            &input,
        );
        assert!(
            output.status.success(),
            "broken Claude asset exited unsuccessfully"
        );
        let line = output
            .requests
            .into_iter()
            .next()
            .expect("broken asset sent a request");
        serde_json::from_str(&line).expect("decode captured API request")
    }
}
