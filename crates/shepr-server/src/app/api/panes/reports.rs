use super::*;
use shepr_agent::resume::ReportedSessionStart;

impl App {
    pub(crate) fn handle_pane_report_agent(
        &mut self,
        params: PaneReportAgentParams,
    ) -> shepr_api::error::ApiResult {
        let pane_id = self.json_pane(&params.pane_id)?;
        let (origin, session_ref) = Self::parse_agent_report_identity(
            &params.source,
            params.agent_session_id,
            params.agent_session_path,
        )?;
        let sample = self.hook_clock_sample();
        self.handle_api_report(crate::app::events::ApiReport::hook_state(
            pane_id,
            sample,
            origin,
            detect_state_from_api(params.state),
            params.seq,
            session_ref,
        ));

        // A parked or rejected report is still answered with success: hooks
        // are fire-and-forget, and the admission outcome is logged where it
        // is decided (`admit_hook_outcome`).
        Ok(ResponseResult::Ok {})
    }

    pub(crate) fn handle_pane_report_agent_session(
        &mut self,
        params: PaneReportAgentSessionParams,
    ) -> shepr_api::error::ApiResult {
        let pane_id = self.json_pane(&params.pane_id)?;
        let (origin, session_ref) = Self::parse_agent_report_identity(
            &params.source,
            params.agent_session_id,
            params.agent_session_path,
        )?;
        // The report's own source was validated above. An unknown session
        // start source from that integration keeps the report: its session
        // identity is what resume on restore needs, and agents can send new
        // start values before shepr knows them. It is not an omitted start
        // either; the policy never lets it replace a session
        // (`ReportedSessionStart`).
        let session_start_source = match params.session_start_source {
            Some(Ok(source)) => ReportedSessionStart::Known(source),
            Some(Err(source)) => {
                shepr_platform::structured_log!(
                    WARN, event = agent.session_start, outcome = Refused,
                    pane = %pane_id,
                    public_pane_id = %params.pane_id,
                    agent = %origin.agent(),
                    reported_start_source = source.as_str(),
                    seq = ?params.seq,
                    session_ref = ?session_ref,
                    "agent integration reported an unknown session start source; \
                     recording the session without letting it replace one"
                );
                ReportedSessionStart::Unrecognized
            }
            None => ReportedSessionStart::Omitted,
        };
        let sample = self.hook_clock_sample();
        self.handle_api_report(crate::app::events::ApiReport::agent_session(
            pane_id,
            sample,
            origin,
            params.seq,
            session_ref,
            session_start_source,
        ));

        Ok(ResponseResult::Ok {})
    }

    /// The server clock a hook report is admitted at, as the event path
    /// samples it for queued reports.
    fn hook_clock_sample(&self) -> shepr_detect::ownership::HookClockSample {
        self.clock.hook_sample()
    }

    /// Decode the wire identity once; internal events carry its resolved owner.
    fn parse_agent_report_identity(
        source_text: &str,
        id: Option<String>,
        path: Option<String>,
    ) -> Result<
        (
            shepr_agent::ReportOrigin,
            Option<shepr_agent::resume::AgentSessionRef>,
        ),
        shepr_api::error::ApiError,
    > {
        let origin = match shepr_agent::ReportOrigin::parse(source_text) {
            Ok(origin) => origin,
            Err(shepr_agent::ReportOriginError::UnsupportedSource) => {
                return failure(
                    ApiErrorCode::InvalidAgent,
                    "report source is not a bundled shepr integration",
                );
            }
        };
        let session_ref = parse_origin_session_ref(&origin, id, path)?;
        Ok((origin, session_ref))
    }
}

/// Raw JSON options stop here. A supplied reference must select one kind.
fn parse_origin_session_ref(
    origin: &shepr_agent::ReportOrigin,
    id: Option<String>,
    path: Option<String>,
) -> Result<Option<shepr_agent::resume::AgentSessionRef>, shepr_api::error::ApiError> {
    if id.is_some() && path.is_some() {
        return failure(
            ApiErrorCode::InvalidRequest,
            "supply either agent_session_id or agent_session_path, not both",
        );
    }
    let supplied = id.is_some() || path.is_some();
    let session_ref = shepr_agent::resume::session_ref_for_agent_report(origin.agent(), id, path);
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
    source: &str,
    id: Option<String>,
    path: Option<String>,
) -> Result<Option<shepr_agent::resume::AgentSessionRef>, shepr_api::error::ApiError> {
    let (_, session) = App::parse_agent_report_identity(source, id, path)?;
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::test_support::{IsolatedEnv, ScratchDir};
    use shepr_api::schema::{Method, Request};

    #[test]
    fn hook_reports_distinguish_malformed_and_missing_pane_ids() {
        let _env = IsolatedEnv::new();
        let app = App::new(&shepr_config::ServerConfig::default());
        assert_eq!(
            app.json_pane("bad").expect_err("malformed pane id").code,
            ApiErrorCode::InvalidPaneId,
        );
        assert_eq!(
            app.json_pane("w99:p99").expect_err("missing pane").code,
            ApiErrorCode::PaneNotFound,
        );
    }

    #[test]
    fn a_report_cannot_select_two_session_reference_kinds() {
        let error = App::parse_agent_report_identity(
            "shepr:pi",
            Some("session-id".into()),
            Some("/sessions/pi.jsonl".into()),
        )
        .expect_err("ambiguous reference");
        assert_eq!(error.code, ApiErrorCode::InvalidRequest);
    }

    #[test]
    fn unsupported_sources_fail_before_dispatch() {
        for source in ["shepr:claud", "custom:status", "custom:pi", "myagent"] {
            let error = App::parse_agent_report_identity(source, Some("id".into()), None)
                .expect_err("only bundled integrations report");
            assert_eq!(error.code, ApiErrorCode::InvalidAgent, "{source}");
            assert!(
                error
                    .into_message()
                    .contains("not a bundled shepr integration"),
                "{source}"
            );
        }
    }

    #[test]
    fn missing_session_ref_is_distinct_from_invalid_supplied_ref() {
        assert!(
            parse_report_session_ref("shepr:kimi", None, None)
                .expect("state-only report")
                .is_none()
        );
        assert!(parse_report_session_ref("shepr:kimi", Some(String::new()), None).is_err());
        assert!(
            parse_report_session_ref("shepr:kimi", None, Some("/session.jsonl".into())).is_err()
        );
    }

    #[test]
    fn official_report_accepts_a_supported_path_session() {
        let session_ref =
            parse_report_session_ref("shepr:pi", None, Some("/sessions/pi.jsonl".into()))
                .expect("supported path session")
                .expect("report carries a session reference");

        assert_eq!(
            session_ref.kind(),
            shepr_agent::resume::AgentSessionRefKind::Path
        );
        assert_eq!(session_ref.value_str(), "/sessions/pi.jsonl");
    }

    #[test]
    fn broken_agent_asset_is_rejected_by_the_report_handler() {
        let _environment = IsolatedEnv::new();
        let scratch = ScratchDir::new("agent-report-handler-mutation");
        let asset = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../shepr-integration/src/assets/claude/shepr-agent-state.sh");
        let source = std::fs::read_to_string(&asset).expect("read Claude integration asset");
        let broken = source.replacen("SOURCE = \"shepr:claude\"", "SOURCE = \"custom:claude\"", 1);
        assert_ne!(broken, source, "mutation probe must change the asset");
        let broken_path = scratch.join("broken-claude-state.sh");
        std::fs::write(&broken_path, broken).expect("write broken asset copy in scratch");

        let detected = shepr_detect::ownership::HookClockSample {
            monotonic: std::time::Instant::now(),
            wall: std::time::SystemTime::now(),
        };
        let mut app = crate::agent_report_test_support::AgentReportHarness::new(
            scratch.path(),
            shepr_agent::Agent::Claude,
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
                shepr_detect::ownership::HookClockSample {
                    monotonic: detected.monotonic + std::time::Duration::from_millis(1),
                    wall: detected.wall + std::time::Duration::from_millis(1),
                },
            )
            .expect_err("report handler rejects an unsupported source");
        assert_eq!(error.code, ApiErrorCode::InvalidAgent);
        assert!(error.into_message().contains("not a bundled"));
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
        let output = shepr_test_support::capture_hook(command, &socket_path, pane_id, &input);
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
