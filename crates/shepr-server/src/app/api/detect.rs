use shepr_api::error::{ApiError, ApiErrorCode, ApiResult};
use shepr_api::schema::{
    DetectionCapture, DetectionExplanation, PaneTarget, ResponseResult, ScreenDetectionSkipReason,
};

use crate::app::App;

use super::super::api_helpers::pane_not_found;
use super::responses::{failure, success};

/// One locked read of the detector's input, the same read the live detection
/// tick takes, so the screen and OSC values describe one terminal state.
fn detection_capture(pane: &shepr_mux::pane::PaneRuntime) -> DetectionCapture {
    let inputs = pane.read().agent_detection_inputs();
    DetectionCapture {
        screen: inputs.screen_text,
        osc_title: inputs.osc_title,
        osc_progress: inputs.osc_progress,
    }
}

impl App {
    /// The detector input for one pane. It works on any pane, agent detected
    /// or not: capturing an agent the manifests do not recognise yet is exactly
    /// when manifest work needs it. The screen is the whole detection snapshot,
    /// never the scrolled viewport.
    pub(super) fn handle_detect_capture(&mut self, target: &PaneTarget) -> ApiResult {
        let Ok(public_id) = target.pane_id.parse::<shepr_protocol::PublicPaneId>() else {
            return Err(ApiError::new(
                ApiErrorCode::InvalidPaneId,
                format!(
                    "invalid pane id {:?}; expected w<workspace>:p<pane>",
                    target.pane_id
                ),
            ));
        };
        let Some((ws_idx, pane_id)) = self.resolve_pane_id(&public_id) else {
            return Err(pane_not_found(&target.pane_id));
        };
        let Some(pane) = self.lookup_runtime(ws_idx, pane_id) else {
            return Err(self.detect_terminal_unavailable_error(ws_idx, pane_id, &target.pane_id));
        };

        success(ResponseResult::DetectCapture {
            pane_id: public_id,
            capture: detection_capture(pane),
        })
    }

    /// Explains what the detector concludes for one pane from the same input
    /// the live detector reads. A pane whose effective state comes from hook
    /// authority skips screen detection unless a visible blocker overrides the
    /// hook report, so it answers with that source instead of rule evidence.
    pub(super) fn handle_detect_explain(&mut self, target: &PaneTarget) -> ApiResult {
        let (ws_idx, pane_id) = self.json_pane(&target.pane_id)?;
        let Some(terminal) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
            .and_then(|terminal_id| self.state.terminals.get(terminal_id))
        else {
            return Err(pane_not_found(&target.pane_id));
        };
        // Keep detect explain's runtime requirement even when hook authority
        // can describe the state; failed restores keep the same
        // pane_terminal_unavailable response as detect capture.
        let Some(pane) = self.lookup_runtime(ws_idx, pane_id) else {
            return Err(self.detect_terminal_unavailable_error(ws_idx, pane_id, &target.pane_id));
        };
        let owner = terminal.ownership().state_owner();
        if let Some(authority) = terminal.ownership().hook_authority().filter(|_| {
            matches!(
                owner,
                shepr_detect::ownership::EffectiveStateSource::FullLifecycleHook
                    | shepr_detect::ownership::EffectiveStateSource::Hook
            )
        }) {
            let skip_reason =
                if owner == shepr_detect::ownership::EffectiveStateSource::FullLifecycleHook {
                    ScreenDetectionSkipReason::FullLifecycleHookAuthority
                } else {
                    ScreenDetectionSkipReason::HookAuthority
                };
            let explain = DetectionExplanation::hook_authority(
                authority.origin.agent().label(),
                terminal.ownership().state(),
                authority.origin.source().as_str(),
                skip_reason,
            );
            return success(ResponseResult::DetectExplain { explain });
        }
        let Some(agent) = terminal
            .ownership()
            .effective_agent()
            .or(terminal.ownership().detected_agent())
        else {
            return failure(
                ApiErrorCode::AgentExplainUnavailable,
                format!(
                    "pane {} does not have a detected agent label",
                    target.pane_id
                ),
            );
        };

        let capture = detection_capture(pane);
        let explain = shepr_detect::manifest::explain_with_input(
            agent,
            shepr_detect::manifest::DetectionInput {
                screen: &capture.screen,
                osc_title: &capture.osc_title,
                osc_progress: &capture.osc_progress,
            },
        );
        success(ResponseResult::DetectExplain {
            explain: explain.into(),
        })
    }

    fn detect_terminal_unavailable_error(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
        public_pane_id: &str,
    ) -> ApiError {
        let restore_failure = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
            .and_then(|terminal_id| self.state.terminals.get(terminal_id))
            .and_then(|terminal| terminal.restore_error());
        let message = match restore_failure {
            Some(failure) => format!("pane {public_pane_id} has no running terminal: {failure}"),
            None => format!("pane {public_pane_id} has no running terminal"),
        };
        ApiError::new(ApiErrorCode::PaneTerminalUnavailable, message)
    }
}

#[cfg(test)]
mod tests {
    use crate::app::App;
    use crate::test_support::*;
    use shepr_agent::{Agent, AgentState};
    use shepr_api::schema::{AppMethod, AppRequest, PaneTarget};

    fn app_with_pane(name: &str) -> (App, shepr_core::layout::PaneId) {
        let mut app = App::new(
            &shepr_config::ServerConfig::default(),
            crate::app::AppPolicy::Suspended,
        );
        app.state
            .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new(name)]);
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].root_pane();
        (app, pane_id)
    }

    fn request(app: &mut App, id: &str, method: AppMethod) -> serde_json::Value {
        let response = app.handle_api_request(AppRequest {
            id: id.into(),
            method,
        });
        serde_json::from_str(&test_json(&response)).expect("test precondition")
    }

    #[tokio::test]
    async fn explain_evaluates_with_server_manifest_cache() {
        let (mut app, pane_id) = app_with_pane("detect-explain");
        let terminal_id = app.state.workspaces[0].panes()[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .ownership_mut()
            .set_detected_agent_process_at(Agent::Codex, std::time::Instant::now());
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(
            80,
            24,
            b"press enter to confirm or esc to cancel",
        );
        app.terminal_runtimes.insert(terminal_id, runtime);
        let pane = app
            .public_pane_id(0, pane_id)
            .expect("test precondition")
            .to_string();

        let response = request(
            &mut app,
            "detect_explain",
            AppMethod::DetectExplain(PaneTarget { pane_id: pane }),
        );

        assert_eq!(response["result"]["type"], "detect_explain");
        assert_eq!(response["result"]["explain"]["state"], "blocked");
        assert_eq!(
            response["result"]["explain"]["matched_rule"]["id"],
            "live_strong_blocker"
        );
    }

    #[tokio::test]
    async fn capture_reads_the_snapshot_that_explain_evaluates() {
        let (mut app, pane_id) = app_with_pane("detect-capture");
        let terminal_id = app.state.workspaces[0].panes()[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .ownership_mut()
            .set_detected_agent_process_at(Agent::Codex, std::time::Instant::now());
        let runtime =
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"captured screen");
        runtime.test_process_pty_bytes(b"\x1b]2;Action Required\x1b\\\x1b]9;4;3;\x1b\\");
        let detection_text = runtime.read().detection_text();
        app.terminal_runtimes.insert(terminal_id, runtime);
        let pane = app
            .public_pane_id(0, pane_id)
            .expect("test precondition")
            .to_string();

        let capture = request(
            &mut app,
            "detect_capture",
            AppMethod::DetectCapture(PaneTarget {
                pane_id: pane.clone(),
            }),
        );
        assert_eq!(capture["result"]["type"], "detect_capture");
        assert_eq!(capture["result"]["pane_id"], pane);
        assert_eq!(
            capture["result"]["capture"]["screen"].as_str(),
            Some(detection_text.as_str())
        );
        assert_eq!(capture["result"]["capture"]["osc_title"], "Action Required");
        assert_eq!(capture["result"]["capture"]["osc_progress"], "4;3;");
        assert!(
            detection_text.contains("captured screen"),
            "{detection_text:?}"
        );

        let explain = request(
            &mut app,
            "detect_explain",
            AppMethod::DetectExplain(PaneTarget { pane_id: pane }),
        );
        assert_eq!(explain["result"]["explain"]["state"], "blocked");
        assert_eq!(
            explain["result"]["explain"]["matched_rule"]["id"],
            "osc_title_blocked"
        );
    }

    #[tokio::test]
    async fn capture_works_on_a_pane_with_no_detected_agent() {
        let (mut app, pane_id) = app_with_pane("detect-capture-plain");
        let terminal_id = app.state.workspaces[0].panes()[&pane_id]
            .attached_terminal_id
            .clone();
        let runtime =
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"an unknown agent");
        app.terminal_runtimes.insert(terminal_id, runtime);
        let pane = app
            .public_pane_id(0, pane_id)
            .expect("test precondition")
            .to_string();

        let capture = request(
            &mut app,
            "capture_plain",
            AppMethod::DetectCapture(PaneTarget {
                pane_id: pane.clone(),
            }),
        );
        assert!(
            capture["result"]["capture"]["screen"]
                .as_str()
                .is_some_and(|text| text.contains("an unknown agent")),
            "{capture}"
        );

        let explain = request(
            &mut app,
            "explain_plain",
            AppMethod::DetectExplain(PaneTarget { pane_id: pane }),
        );
        assert_eq!(explain["error"]["code"], "agent_explain_unavailable");
    }

    #[tokio::test]
    async fn detect_rejects_an_unknown_pane_and_resolves_no_agent_labels() {
        let (mut app, pane_id) = app_with_pane("detect-unknown");
        let terminal_id = app.state.workspaces[0].panes()[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition");
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.terminal_runtimes.insert(
            terminal_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
        );

        for name in ["pi", "w9:p9"] {
            for method in [
                AppMethod::DetectCapture(PaneTarget {
                    pane_id: name.into(),
                }),
                AppMethod::DetectExplain(PaneTarget {
                    pane_id: name.into(),
                }),
            ] {
                let response = request(&mut app, "unknown", method);
                let expected = if name == "pi" {
                    "invalid_pane_id"
                } else {
                    "pane_not_found"
                };
                assert_eq!(response["error"]["code"], expected, "{name}");
            }
        }
    }

    #[tokio::test]
    async fn detect_on_a_pane_without_a_terminal_reports_why_it_has_none() {
        let (mut app, pane_id) = app_with_pane("detect-no-terminal");
        let terminal_id = app.state.workspaces[0].panes()[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition");
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        terminal.record_start_failure(shepr_mux::terminal::PaneStartFailure::shell_start_failed(
            &std::io::Error::from(std::io::ErrorKind::NotFound),
        ));
        let pane = app
            .public_pane_id(0, pane_id)
            .expect("test precondition")
            .to_string();

        for method in [
            AppMethod::DetectCapture(PaneTarget {
                pane_id: pane.clone(),
            }),
            AppMethod::DetectExplain(PaneTarget {
                pane_id: pane.clone(),
            }),
        ] {
            let response = request(&mut app, "no_terminal", method);
            assert_eq!(
                response["error"]["code"], "pane_terminal_unavailable",
                "{response}"
            );
            let message = response["error"]["message"].as_str().unwrap_or_default();
            assert!(message.contains(&pane), "{response}");
            assert!(
                message.contains("Could not start the pane shell"),
                "{response}"
            );
        }
    }

    #[tokio::test]
    async fn explain_reports_the_hook_authority_skip_for_a_hook_owned_pane() {
        let (mut app, pane_id) = app_with_pane("detect-explain-omp");
        let terminal_id = app.state.workspaces[0].panes()[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition");
        // Full lifecycle authority needs a live detected agent and an anchored
        // session, exactly as a real hook-owned pane has.
        let scratch = ScratchDir::new("detect-explain-omp-session");
        let session_ref = shepr_agent::resume::AgentSessionRef::path(
            scratch.join("session.jsonl").display().to_string(),
        )
        .expect("test precondition");
        terminal.set_detected_state(Some(Agent::Omp), AgentState::Idle);
        terminal.ownership_mut().set_persisted_agent_session(
            shepr_agent::resume::PersistedAgentSession::from_report(
                "shepr:omp",
                "omp",
                session_ref.clone(),
            )
            .expect("test precondition"),
        );
        terminal
            .set_hook_report_at(
                shepr_agent::ReportOrigin::parse("shepr:omp", "omp").expect("test origin"),
                AgentState::Working,
                Some(session_ref),
                Some(1),
                shepr_detect::ownership::HookClockSample {
                    monotonic: std::time::Instant::now(),
                    wall: std::time::SystemTime::now(),
                },
            )
            .expect("test precondition");
        app.terminal_runtimes.insert(
            terminal_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
        );
        let pane = app
            .public_pane_id(0, pane_id)
            .expect("test precondition")
            .to_string();

        let response = request(
            &mut app,
            "detect_explain_omp",
            AppMethod::DetectExplain(PaneTarget { pane_id: pane }),
        );

        let explain = &response["result"]["explain"];
        assert_eq!(
            explain["state_source"]["kind"], "hook_authority",
            "{response}"
        );
        assert_eq!(
            explain["state_source"]["skip_reason"], "full_lifecycle_hook_authority",
            "{response}"
        );
        assert_eq!(explain["state"], "working", "{response}");
    }
}
