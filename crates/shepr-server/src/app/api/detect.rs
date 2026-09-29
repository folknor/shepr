use shepr_api::error::{ApiErrorCode, ApiResult};
use shepr_api::schema::{PaneTarget, ResponseResult};

use crate::app::App;

use super::super::api_helpers::pane_not_found;
use super::responses::{failure, success};

impl App {
    /// The text the detector reads for one pane. It works on any pane, agent
    /// detected or not: capturing an agent the manifests do not recognise yet
    /// is exactly when manifest work needs it. It is the detection snapshot,
    /// never the scrolled viewport, plain text and whole.
    pub(super) fn handle_detect_capture(&mut self, target: &PaneTarget) -> ApiResult {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&target.pane_id) else {
            return Err(pane_not_found(Some(&target.pane_id)));
        };
        let Some(public_pane_id) = self.public_pane_id(ws_idx, pane_id) else {
            return Err(pane_not_found(Some(&target.pane_id)));
        };
        let Some((pane, _workspace_id)) = self.lookup_runtime(ws_idx, pane_id) else {
            return Err(pane_not_found(Some(&target.pane_id)));
        };

        success(ResponseResult::DetectCapture {
            pane_id: public_pane_id,
            text: pane.detection_text(),
        })
    }

    /// Explains what the detector concludes for one pane from the same input
    /// the live detector reads. A pane whose agent state is owned by full
    /// lifecycle hooks skips screen detection, so it answers with that skip
    /// instead of rule evidence.
    pub(super) fn handle_detect_explain(&mut self, target: &PaneTarget) -> ApiResult {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&target.pane_id) else {
            return Err(pane_not_found(Some(&target.pane_id)));
        };
        let Some((pane, _workspace_id)) = self.lookup_runtime(ws_idx, pane_id) else {
            return Err(pane_not_found(Some(&target.pane_id)));
        };
        let Some(terminal) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
            .and_then(|terminal_id| self.state.terminals.get(terminal_id))
        else {
            return Err(pane_not_found(Some(&target.pane_id)));
        };
        if terminal.full_lifecycle_hook_authority_active() {
            let explain = serde_json::json!({
                "agent": terminal.effective_agent_label().unwrap_or("unknown"),
                "state": shepr_agent::detect::manifest::agent_state_label(terminal.state),
                "matched_rule": null,
                "visible_idle": false,
                "visible_blocker": false,
                "visible_working": false,
                "screen_detection_skipped": true,
                "screen_detection_skip_reason": "full_lifecycle_hook_authority",
                "skip_state_update": false,
                "skipped_update_reason": null,
                "fallback_reason": null,
                "evaluated_rules": [],
            });
            return success(ResponseResult::DetectExplain { explain });
        }
        let Some(agent) = terminal.effective_known_agent().or(terminal.detected_agent) else {
            return failure(
                ApiErrorCode::AgentExplainUnavailable,
                format!(
                    "pane {} does not have a detected agent label",
                    target.pane_id
                ),
            );
        };

        let screen = pane.detection_text();
        let osc_title = pane.agent_osc_title();
        let osc_progress = pane.agent_osc_progress();
        let explain = shepr_agent::detect::manifest::explain_with_input(
            agent,
            shepr_agent::detect::manifest::DetectionInput {
                screen: &screen,
                osc_title: &osc_title,
                osc_progress: &osc_progress,
            },
        );
        let value = shepr_agent::detect::manifest::explain_to_json_value(&explain);

        success(ResponseResult::DetectExplain { explain: value })
    }
}

#[cfg(test)]
mod tests {
    use crate::app::App;
    use crate::test_support::*;
    use shepr_agent::detect::{Agent, AgentState};
    use shepr_api::schema::{AppMethod, AppRequest, PaneTarget};

    fn app_with_pane(name: &str) -> (App, shepr_core::layout::PaneId) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &shepr_config::Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
        );
        app.state.workspaces = vec![shepr_mux::workspace::Workspace::test_new(name)];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
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
        let terminal_id = app.state.workspaces[0].tabs()[0].panes()[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .detected_agent = Some(Agent::Codex);
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
        let terminal_id = app.state.workspaces[0].tabs()[0].panes()[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .detected_agent = Some(Agent::Codex);
        let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(
            80,
            24,
            b"press enter to confirm or esc to cancel",
        );
        let detection_text = runtime.detection_text();
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
            capture["result"]["text"].as_str(),
            Some(detection_text.as_str())
        );
        assert!(
            detection_text.contains("press enter to confirm"),
            "{detection_text:?}"
        );

        let explain = request(
            &mut app,
            "detect_explain",
            AppMethod::DetectExplain(PaneTarget { pane_id: pane }),
        );
        assert_eq!(explain["result"]["explain"]["state"], "blocked");
    }

    #[tokio::test]
    async fn capture_works_on_a_pane_with_no_detected_agent() {
        let (mut app, pane_id) = app_with_pane("detect-capture-plain");
        let terminal_id = app.state.workspaces[0].tabs()[0].panes()[&pane_id]
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
            capture["result"]["text"]
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
        let terminal_id = app.state.workspaces[0].tabs()[0].panes()[&pane_id]
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
                assert_eq!(response["error"]["code"], "pane_not_found", "{name}");
            }
        }
    }

    #[tokio::test]
    async fn explain_reports_the_hook_authority_skip_for_a_hook_owned_pane() {
        let (mut app, pane_id) = app_with_pane("detect-explain-omp");
        let terminal_id = app.state.workspaces[0].tabs()[0].panes()[&pane_id]
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
        let session_ref = shepr_agent::agent::resume::AgentSessionRef::path(
            scratch.join("session.jsonl").display().to_string(),
        )
        .expect("test precondition");
        terminal.set_detected_state(Some(Agent::Omp), AgentState::Idle);
        terminal.set_persisted_agent_session(
            shepr_agent::agent::resume::PersistedAgentSession::from_report(
                "shepr:omp",
                "omp",
                session_ref,
            )
            .expect("test precondition"),
        );
        terminal.set_hook_authority(
            "shepr:omp".to_string(),
            "omp".to_string(),
            AgentState::Working,
            None,
            Some(1),
        );
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
        assert_eq!(explain["screen_detection_skipped"], true, "{response}");
        assert_eq!(
            explain["screen_detection_skip_reason"], "full_lifecycle_hook_authority",
            "{response}"
        );
        assert_eq!(explain["state"], "working", "{response}");
    }
}
