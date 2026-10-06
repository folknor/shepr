use shepr_api::error::{ApiError, ApiErrorCode, ApiResult};
use shepr_api::schema::{
    DetectionCapture, DetectionExplanation, PaneTarget, ResponseResult, ScreenDetectionGate,
    ScreenDetectionSkipReason, UnappliedHookReport,
};

use crate::app::App;

use super::responses::failure;

/// One locked read of the detector's input, the same read the live detection
/// tick takes, so the screen and OSC values describe one terminal state.
fn detection_capture(
    pane: &shepr_mux::pane::PaneRuntime,
) -> Result<DetectionCapture, shepr_mux::pane::AgentDetectionReadError> {
    let inputs = pane.read().agent_detection_inputs()?;
    Ok(DetectionCapture {
        screen: inputs.screen_text,
        osc_title: inputs.osc_title.unwrap_or_default(),
        osc_progress: inputs.osc_progress.unwrap_or_default(),
    })
}

fn detection_read_error(
    public_pane_id: &str,
    error: shepr_mux::pane::AgentDetectionReadError,
) -> ApiError {
    ApiError::new(
        ApiErrorCode::PaneTerminalUnavailable,
        format!("pane {public_pane_id} detection input read failed: {error}"),
    )
}

fn screen_detection_gate(gate: shepr_mux::pane::DetectorGate) -> ScreenDetectionGate {
    match gate {
        shepr_mux::pane::DetectorGate::PendingIdleConfirmation => {
            ScreenDetectionGate::PendingIdleConfirmation
        }
        shepr_mux::pane::DetectorGate::StartupGrace => ScreenDetectionGate::StartupGrace,
        shepr_mux::pane::DetectorGate::ResumeAbsenceHold => ScreenDetectionGate::ResumeAbsenceHold,
    }
}

impl App {
    /// The detector input for one pane. It works on any pane, agent detected
    /// or not: capturing an agent the manifests do not recognise yet is exactly
    /// when manifest work needs it. The screen is the whole detection snapshot,
    /// never the scrolled viewport.
    pub(super) fn handle_detect_capture(&mut self, target: &PaneTarget) -> ApiResult {
        let (public_id, pane_id) = self.json_pane_with_id(&target.pane_id)?;
        let Some(pane) = self.lookup_runtime(pane_id) else {
            return Err(self.detect_terminal_unavailable_error(pane_id, &target.pane_id));
        };

        let capture = detection_capture(pane)
            .map_err(|error| detection_read_error(&target.pane_id, error))?;
        Ok(ResponseResult::DetectCapture {
            pane_id: public_id,
            capture,
        })
    }

    /// Reports the pane's effective state and source beside a fresh manifest
    /// evaluation of the same screen input the live detector reads. Hook
    /// authority includes its screen-detection skip reason. The answer also
    /// carries the pane's last parked or rejected hook report.
    pub(super) fn handle_detect_explain(&mut self, target: &PaneTarget) -> ApiResult {
        let pane_id = self.json_pane(&target.pane_id)?;
        let Some(terminal) = self.state.terminal(pane_id) else {
            return Err(ApiError::pane_not_found(&target.pane_id));
        };
        // Keep detect explain's runtime requirement even when hook authority
        // can describe the state; failed restores keep the same
        // pane_terminal_unavailable response as detect capture.
        let Some(pane) = self.lookup_runtime(pane_id) else {
            return Err(self.detect_terminal_unavailable_error(pane_id, &target.pane_id));
        };
        let now = self.clock.now;
        let last_unapplied_hook_report = terminal
            .ownership()
            .last_unapplied_hook_report(now)
            .map(|report| UnappliedHookReport::from_ownership(&report, now));
        let ownership = terminal.ownership();
        let owner = ownership.state_owner();
        let Some(agent) = ownership.effective_agent().or(ownership.detected_agent()) else {
            return failure(
                ApiErrorCode::AgentExplainUnavailable,
                format!(
                    "pane {} does not have a detected agent label",
                    target.pane_id
                ),
            );
        };

        let detector_gate = if owner == shepr_detect::ownership::EffectiveStateSource::Screen {
            pane.active_detector_gate(now).map(screen_detection_gate)
        } else {
            None
        };
        let capture = detection_capture(pane)
            .map_err(|error| detection_read_error(&target.pane_id, error))?;
        let screen_explain = shepr_detect::manifest::explain_with_input(
            agent,
            shepr_detect::manifest::DetectionInput {
                screen: &capture.screen,
                osc_title: capture.osc_title_evidence(),
                osc_progress: capture.osc_progress_evidence(),
            },
        );
        // Effective ownership, the latest mux gate, and a fresh screen
        // evaluation answer separate questions.
        let state_source = match owner {
            shepr_detect::ownership::EffectiveStateSource::Screen => {
                shepr_api::schema::DetectionStateSource::Screen
            }
            shepr_detect::ownership::EffectiveStateSource::ProcessExit => {
                shepr_api::schema::DetectionStateSource::ProcessExit
            }
            shepr_detect::ownership::EffectiveStateSource::Hook
            | shepr_detect::ownership::EffectiveStateSource::FullLifecycleHook => {
                match ownership.hook_authority() {
                    Some(authority) => {
                        let skip_reason = if owner
                            == shepr_detect::ownership::EffectiveStateSource::FullLifecycleHook
                        {
                            ScreenDetectionSkipReason::FullLifecycleHookAuthority
                        } else {
                            ScreenDetectionSkipReason::HookAuthority
                        };
                        shepr_api::schema::DetectionStateSource::HookAuthority {
                            hook_source: authority.origin.source().as_str().to_owned(),
                            skip_reason,
                        }
                    }
                    None => {
                        tracing::error!(
                            public_pane_id = %target.pane_id,
                            "pane state owner names hook authority without an authority"
                        );
                        shepr_api::schema::DetectionStateSource::Screen
                    }
                }
            }
        };
        Ok(ResponseResult::DetectExplain {
            explain: Box::new(
                DetectionExplanation::from(screen_explain)
                    .with_pane_decision(ownership.state(), state_source)
                    .with_detector_gate(detector_gate)
                    .with_last_unapplied_hook_report(last_unapplied_hook_report),
            ),
        })
    }

    fn detect_terminal_unavailable_error(
        &self,
        pane_id: shepr_core::layout::PaneId,
        public_pane_id: &str,
    ) -> ApiError {
        let restore_failure = self
            .state
            .terminal(pane_id)
            .and_then(|terminal| terminal.start_failure());
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

    fn app_with_pane(name: &str) -> (crate::app::TestApp, shepr_core::layout::PaneId) {
        let mut app = App::new(&shepr_config::ServerConfig::default());
        app.state
            .test_set_workspaces(vec![shepr_mux::workspace::Workspace::test_new(name)]);
        let pane_id = app.state.ws(0).tree().root();
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
        app.state
            .terminal_mut(pane_id)
            .ownership_mut()
            .set_detected_agent_process_at(Agent::Codex, std::time::Instant::now());
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(
            80,
            24,
            b"press enter to confirm or esc to cancel",
        );
        app.terminal_runtimes.insert(pane_id, runtime);
        let pane = app
            .state
            .pane(pane_id)
            .expect("test precondition")
            .public_id()
            .to_string();

        let response = request(
            &mut app,
            "detect_explain",
            AppMethod::DetectExplain(PaneTarget { pane_id: pane }),
        );

        assert_eq!(response["result"]["type"], "detect_explain");
        assert_eq!(response["result"]["explain"]["state"], "unknown");
        assert_eq!(response["result"]["explain"]["screen_state"], "blocked");
        assert_eq!(
            response["result"]["explain"]["matched_rule"]["id"],
            "live_strong_blocker"
        );
    }

    #[tokio::test]
    async fn capture_reads_the_snapshot_that_explain_evaluates() {
        let (mut app, pane_id) = app_with_pane("detect-capture");
        app.state
            .terminal_mut(pane_id)
            .ownership_mut()
            .set_detected_agent_process_at(Agent::Codex, std::time::Instant::now());
        let runtime =
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"captured screen");
        runtime.test_process_pty_bytes(b"\x1b]2;Action Required\x1b\\\x1b]9;4;3;\x1b\\");
        let detection_text = runtime.read().detection_text();
        app.terminal_runtimes.insert(pane_id, runtime);
        let pane = app
            .state
            .pane(pane_id)
            .expect("test precondition")
            .public_id()
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
        assert_eq!(capture["result"]["capture"]["osc_progress"], "4;3");
        assert!(
            detection_text.contains("captured screen"),
            "{detection_text:?}"
        );

        let explain = request(
            &mut app,
            "detect_explain",
            AppMethod::DetectExplain(PaneTarget { pane_id: pane }),
        );
        assert_eq!(explain["result"]["explain"]["screen_state"], "blocked");
        assert_eq!(
            explain["result"]["explain"]["matched_rule"]["id"],
            "osc_title_blocked"
        );
    }

    #[tokio::test]
    async fn capture_and_explain_report_a_poisoned_screen_read() {
        let (mut app, pane_id) = app_with_pane("detect-read-failure");
        app.state
            .terminal_mut(pane_id)
            .set_detected_state(Some(Agent::Codex), AgentState::Working);
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"ignored");
        runtime.test_break_terminal_core();
        app.terminal_runtimes.insert(pane_id, runtime);
        let pane = app
            .state
            .pane(pane_id)
            .expect("test precondition")
            .public_id()
            .to_string();

        for method in [
            AppMethod::DetectCapture(PaneTarget {
                pane_id: pane.clone(),
            }),
            AppMethod::DetectExplain(PaneTarget {
                pane_id: pane.clone(),
            }),
        ] {
            let response = request(&mut app, "read_failure", method);
            assert_eq!(
                response["error"]["code"], "pane_terminal_unavailable",
                "{response}"
            );
            let message = response["error"]["message"].as_str().unwrap_or_default();
            assert!(message.contains(&pane), "{response}");
            assert!(
                message.contains("detection input read failed"),
                "{response}"
            );
            assert!(
                message.contains("terminal core lock is poisoned"),
                "{response}"
            );
        }
    }

    #[tokio::test]
    async fn explain_reports_the_pane_state_when_the_screen_verdict_differs() {
        let (mut app, pane_id) = app_with_pane("detect-explain-state-differs");
        // This childless fixture has no DetectorTask; the real hold transition
        // is tested beside the mux gate, and this checks that no stale gate is
        // reported when the fixture has none.
        app.state
            .terminal_mut(pane_id)
            .set_detected_state(Some(Agent::Codex), AgentState::Working);
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(
            80,
            24,
            b"press enter to confirm or esc to cancel",
        );
        app.terminal_runtimes.insert(pane_id, runtime);
        let pane = app
            .state
            .pane(pane_id)
            .expect("test precondition")
            .public_id()
            .to_string();

        let response = request(
            &mut app,
            "detect_explain_state_differs",
            AppMethod::DetectExplain(PaneTarget { pane_id: pane }),
        );

        let explain = &response["result"]["explain"];
        assert_eq!(explain["state"], "working", "{response}");
        assert_eq!(explain["state_source"]["kind"], "screen", "{response}");
        assert_eq!(explain["screen_state"], "blocked", "{response}");
        assert!(explain["detector_gate"].is_null(), "{response}");
        assert_eq!(
            explain["matched_rule"]["id"], "live_strong_blocker",
            "{response}"
        );
    }

    #[tokio::test]
    async fn capture_works_on_a_pane_with_no_detected_agent() {
        let (mut app, pane_id) = app_with_pane("detect-capture-plain");
        let runtime =
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"an unknown agent");
        app.terminal_runtimes.insert(pane_id, runtime);
        let pane = app
            .state
            .pane(pane_id)
            .expect("test precondition")
            .public_id()
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
        let terminal = app.state.terminal_mut(pane_id);
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.terminal_runtimes.insert(
            pane_id,
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
        let terminal = app.state.terminal_mut(pane_id);
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        terminal.record_start_failure(shepr_mux::terminal::PaneStartFailure::shell_start_failed(
            &std::io::Error::from(std::io::ErrorKind::NotFound),
        ));
        let pane = app
            .state
            .pane(pane_id)
            .expect("test precondition")
            .public_id()
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

    fn report_codex_state(
        app: &mut App,
        pane_id: shepr_core::layout::PaneId,
        session_ref: Option<shepr_agent::resume::AgentSessionRef>,
        seq: u64,
    ) {
        app.state
            .handle_state_event(crate::app::events::StateEvent::HookStateReported {
                pane_id,
                sample: shepr_detect::ownership::HookClockSample {
                    monotonic: std::time::Instant::now(),
                    wall: std::time::SystemTime::now(),
                },
                origin: shepr_agent::ReportOrigin::parse("shepr:codex").expect("test origin"),
                state: AgentState::Working,
                seq: Some(seq),
                session_ref,
            });
    }

    #[tokio::test]
    async fn explain_shows_the_last_rejected_hook_report_until_one_applies() {
        let (mut app, pane_id) = app_with_pane("detect-explain-rejected");
        app.state
            .terminal_mut(pane_id)
            .set_detected_state(Some(Agent::Codex), AgentState::Idle);
        app.terminal_runtimes.insert(
            pane_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
        );
        let pane = app
            .state
            .pane(pane_id)
            .expect("test precondition")
            .public_id()
            .to_string();

        // Codex state reports must name their session; this one does not.
        report_codex_state(&mut app, pane_id, None, 7);
        let response = request(
            &mut app,
            "explain_rejected",
            AppMethod::DetectExplain(PaneTarget {
                pane_id: pane.clone(),
            }),
        );
        let explain = &response["result"]["explain"];
        assert_eq!(explain["agent"], "codex", "{response}");
        let report = &explain["last_unapplied_hook_report"];
        assert_eq!(report["hook_source"], "shepr:codex", "{response}");
        assert_eq!(report["seq"], 7, "{response}");
        assert_eq!(
            report["report"],
            serde_json::json!({ "kind": "state", "state": "working" }),
            "{response}"
        );
        assert_eq!(
            report["outcome"],
            serde_json::json!({ "kind": "rejected", "reason": "missing_session" }),
            "{response}"
        );

        report_codex_state(
            &mut app,
            pane_id,
            shepr_agent::resume::AgentSessionRef::id("codex-session"),
            8,
        );
        let response = request(
            &mut app,
            "explain_applied",
            AppMethod::DetectExplain(PaneTarget { pane_id: pane }),
        );
        assert!(
            response["result"]["explain"]["last_unapplied_hook_report"].is_null(),
            "{response}"
        );
    }

    #[tokio::test]
    async fn explain_says_what_a_parked_hook_report_awaits() {
        let (mut app, pane_id) = app_with_pane("detect-explain-parked");
        app.state
            .terminal_mut(pane_id)
            .set_detected_state(Some(Agent::Kimi), AgentState::Idle);
        app.terminal_runtimes.insert(
            pane_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
        );
        let pane = app
            .state
            .pane(pane_id)
            .expect("test precondition")
            .public_id()
            .to_string();

        // A state report with no start yet: the process is present, but only a
        // session start can promote the report.
        app.state
            .handle_state_event(crate::app::events::StateEvent::HookStateReported {
                pane_id,
                sample: shepr_detect::ownership::HookClockSample {
                    monotonic: std::time::Instant::now(),
                    wall: std::time::SystemTime::now(),
                },
                origin: shepr_agent::ReportOrigin::parse("shepr:kimi").expect("test origin"),
                state: AgentState::Working,
                seq: Some(10),
                session_ref: shepr_agent::resume::AgentSessionRef::id("kimi-root"),
            });
        let response = request(
            &mut app,
            "explain_parked",
            AppMethod::DetectExplain(PaneTarget { pane_id: pane }),
        );
        let report = &response["result"]["explain"]["last_unapplied_hook_report"];
        assert_eq!(report["hook_source"], "shepr:kimi", "{response}");
        assert_eq!(
            report["outcome"],
            serde_json::json!({
                "kind": "parked",
                "awaiting": { "kind": "session_start" },
            }),
            "{response}"
        );
    }

    #[tokio::test]
    async fn explain_reports_the_hook_authority_skip_for_a_hook_owned_pane() {
        let (mut app, pane_id) = app_with_pane("detect-explain-omp");
        let terminal = app.state.terminal_mut(pane_id);
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
                session_ref.clone(),
            )
            .expect("test precondition"),
        );
        terminal
            .ownership_mut()
            .set_hook_report_at(
                shepr_agent::ReportOrigin::parse("shepr:omp").expect("test origin"),
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
            pane_id,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b""),
        );
        let pane = app
            .state
            .pane(pane_id)
            .expect("test precondition")
            .public_id()
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
