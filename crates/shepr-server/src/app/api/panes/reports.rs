use super::*;
use shepr_agent::agent::resume::ReportedSessionStart;

impl App {
    pub(crate) fn handle_pane_report_agent(
        &mut self,
        params: PaneReportAgentParams,
    ) -> shepr_api::error::ApiResult {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return Err(pane_not_found(&params.pane_id));
        };
        let (origin, session_ref) = Self::parse_agent_report_identity(
            &params.source,
            &params.agent,
            params.agent_session_id,
            params.agent_session_path,
        )?;
        let sample = self.hook_clock_sample();
        self.handle_state_event(crate::app::events::StateEvent::HookStateReported {
            pane_id,
            sample,
            session_ref,
            origin,
            state: detect_state_from_api(params.state),
            seq: params.seq,
        });

        // A parked or rejected report is still answered with success: hooks
        // are fire-and-forget, and the admission outcome is logged where it
        // is decided (`admit_hook_outcome`).
        success(ResponseResult::Ok {})
    }

    pub(crate) fn handle_pane_report_agent_session(
        &mut self,
        params: PaneReportAgentSessionParams,
    ) -> shepr_api::error::ApiResult {
        let Some((_ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return Err(pane_not_found(&params.pane_id));
        };
        let (origin, session_ref) = Self::parse_agent_report_identity(
            &params.source,
            &params.agent,
            params.agent_session_id,
            params.agent_session_path,
        )?;
        // An unknown source keeps the report: its session identity is what
        // resume on restore needs, and agents can send new start values
        // before shepr knows them. It is not an omitted source either; the
        // policy never lets it replace a session (`ReportedSessionStart`).
        let session_start_source = match params.session_start_source {
            Some(Ok(source)) => ReportedSessionStart::Known(source),
            Some(Err(source)) => {
                tracing::warn!(
                    pane_id = %params.pane_id,
                    source = source.as_str(),
                    "agent integration reported an unknown session start source; \
                     recording the session without letting it replace one"
                );
                ReportedSessionStart::Unrecognized
            }
            None => ReportedSessionStart::Omitted,
        };
        let sample = self.hook_clock_sample();
        self.handle_state_event(crate::app::events::StateEvent::AgentSessionReported {
            pane_id,
            sample,
            session_ref,
            origin,
            seq: params.seq,
            session_start_source,
        });

        success(ResponseResult::Ok {})
    }

    /// The server clock a hook report is admitted at, as the event path
    /// samples it for queued reports.
    fn hook_clock_sample(&self) -> shepr_agent::ownership::HookClockSample {
        shepr_agent::ownership::HookClockSample {
            monotonic: self.clock.now,
            wall: self.clock.wall_now,
        }
    }

    /// Decode the wire identity once; internal events carry its resolved owner.
    fn parse_agent_report_identity(
        source_text: &str,
        agent_text: &str,
        id: Option<String>,
        path: Option<String>,
    ) -> Result<
        (
            shepr_agent::agent::ReportOrigin,
            Option<shepr_agent::agent::resume::AgentSessionRef>,
        ),
        shepr_api::error::ApiError,
    > {
        let origin = match shepr_agent::agent::ReportOrigin::parse(source_text, agent_text) {
            Ok(origin) => origin,
            Err(shepr_agent::agent::ReportOriginError::EmptyAgent) => return invalid_agent(),
            Err(shepr_agent::agent::ReportOriginError::MismatchedAgent) => {
                return failure(
                    ApiErrorCode::InvalidAgent,
                    "report source does not match agent label",
                );
            }
            Err(shepr_agent::agent::ReportOriginError::UnknownOfficialSource) => {
                return failure(ApiErrorCode::InvalidAgent, "unknown official report source");
            }
        };
        let session_ref = parse_origin_session_ref(&origin, id, path)?;
        Ok((origin, session_ref))
    }
}

/// Raw JSON options stop here. A supplied reference must select one kind;
/// custom reports cannot mint a resumable identity.
fn parse_origin_session_ref(
    origin: &shepr_agent::agent::ReportOrigin,
    id: Option<String>,
    path: Option<String>,
) -> Result<Option<shepr_agent::agent::resume::AgentSessionRef>, shepr_api::error::ApiError> {
    if id.is_some() && path.is_some() {
        return failure(
            ApiErrorCode::InvalidRequest,
            "supply either agent_session_id or agent_session_path, not both",
        );
    }
    let Some(agent) = origin.official_agent() else {
        return Ok(None);
    };
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
fn parse_report_session_ref(
    source: &shepr_agent::agent::AgentSource,
    agent_label: &str,
    id: Option<String>,
    path: Option<String>,
) -> Result<Option<shepr_agent::agent::resume::AgentSessionRef>, shepr_api::error::ApiError> {
    let (_, session) = App::parse_agent_report_identity(source.as_str(), agent_label, id, path)?;
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::test_support::{IsolatedEnv, ScratchDir};
    use shepr_api::schema::{Method, Request};

    #[test]
    fn a_report_cannot_select_two_session_reference_kinds() {
        for source in ["shepr:pi", "custom:status"] {
            let error = App::parse_agent_report_identity(
                source,
                "pi",
                Some("session-id".into()),
                Some("/sessions/pi.jsonl".into()),
            )
            .expect_err("ambiguous reference");
            assert_eq!(error.code, ApiErrorCode::InvalidRequest);
        }
    }

    #[test]
    fn unknown_official_sources_fail_before_dispatch() {
        let error = App::parse_agent_report_identity("shepr:claud", "claude", None, None)
            .expect_err("misspelled reserved source");
        assert_eq!(error.code, ApiErrorCode::InvalidAgent);
    }

    #[test]
    fn missing_session_ref_is_distinct_from_invalid_supplied_ref() {
        assert!(
            parse_report_session_ref(
                &shepr_agent::agent::AgentSource::parse("shepr:kimi"),
                "kimi",
                None,
                None
            )
            .expect("state-only report")
            .is_none()
        );
        assert!(
            parse_report_session_ref(
                &shepr_agent::agent::AgentSource::parse("shepr:kimi"),
                "kimi",
                Some(String::new()),
                None
            )
            .is_err()
        );
        assert!(
            parse_report_session_ref(
                &shepr_agent::agent::AgentSource::parse("shepr:kimi"),
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
            parse_report_session_ref(
                &shepr_agent::agent::AgentSource::parse("shepr:kimi"),
                "kilo",
                Some("session".into()),
                None
            )
            .is_err()
        );
        assert!(
            parse_report_session_ref(
                &shepr_agent::agent::AgentSource::parse("shepr:kimi"),
                "kimi",
                Some("session".into()),
                None
            )
            .expect("official identity")
            .is_some()
        );
    }

    #[test]
    fn official_report_accepts_a_supported_path_session() {
        let session_ref = parse_report_session_ref(
            &shepr_agent::agent::AgentSource::parse("shepr:pi"),
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
            parse_report_session_ref(
                &shepr_agent::agent::AgentSource::parse("custom:state"),
                "kimi",
                Some("session".into()),
                None
            )
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

        let detected = shepr_agent::ownership::HookClockSample {
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
                shepr_agent::ownership::HookClockSample {
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
                .ownership()
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
