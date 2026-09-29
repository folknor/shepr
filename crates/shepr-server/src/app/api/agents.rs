use shepr_api::error::{ApiError, ApiErrorCode, ApiResult};
use shepr_api::schema::{AgentRenameParams, AgentTarget, PaneReadResult, ResponseResult};

use crate::app::App;

use super::responses::{failure, success};

impl App {
    pub(super) fn handle_agent_list(&mut self) -> ApiResult {
        success(ResponseResult::AgentList {
            agents: self.collect_agent_infos(),
        })
    }

    pub(super) fn handle_agent_get(&mut self, target: &AgentTarget) -> ApiResult {
        let agent = self.agent_info_for_target(&target.target)?;

        success(ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_focus(&mut self, target: &AgentTarget) -> ApiResult {
        let agent = self.focus_agent_target(&target.target)?;

        success(ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_rename(&mut self, params: AgentRenameParams) -> ApiResult {
        let agent = self.rename_agent_target(&params.target, params.name)?;

        success(ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_read(
        &mut self,
        params: &shepr_api::schema::AgentReadParams,
    ) -> ApiResult {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return Err(self.agent_target_error(err)),
        };
        let Some((pane, workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(&params.target);
        };
        let format =
            crate::app::api_helpers::effective_read_format(params.format, params.strip_ansi);
        // A write can land while the snapshot is built. Keep the revision at
        // the start so it never claims to cover output absent from the text.
        let revision = pane.content_seq();
        let snapshot = crate::app::api_helpers::read_terminal_snapshot(
            pane,
            params.source,
            format,
            params.lines,
        )?;
        let Some(tab_id) = self.public_tab_id(resolved.ws_idx, resolved.tab_idx) else {
            return failure(
                ApiErrorCode::TabNotFound,
                "agent pane tab is no longer available",
            );
        };

        success(ResponseResult::PaneRead {
            read: PaneReadResult {
                pane_id: self
                    .public_pane_id(resolved.ws_idx, resolved.pane_id)
                    .unwrap_or_else(|| params.target.clone()),
                workspace_id,
                tab_id,
                source: params.source,
                format,
                text: snapshot.text,
                revision,
                truncated: snapshot.truncated,
            },
        })
    }

    pub(super) fn handle_agent_explain(&mut self, target: &AgentTarget) -> ApiResult {
        let resolved = match self.resolve_agent_target(&target.target) {
            Ok(resolved) => resolved,
            Err(err) => return Err(self.agent_target_error(err)),
        };
        let Some((pane, _workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(&target.target);
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(&target.target);
        };
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return agent_not_found(&target.target);
        };
        if terminal.full_lifecycle_hook_authority_active() {
            let explain = serde_json::json!({
                "agent": terminal.effective_agent_label().unwrap_or("unknown"),
                "state": shepr_agent::detect::manifest::agent_state_label(terminal.state),
                "manifest_source": null,
                "matched_rule": null,
                "visible_idle": false,
                "visible_blocker": false,
                "visible_working": false,
                "screen_detection_skipped": true,
                "screen_detection_skip_reason": "full_lifecycle_hook_authority",
                "skip_state_update": false,
                "skipped_update_reason": null,
                "fallback_reason": null,
                "warning": null,
                "evaluated_rules": [],
            });
            return success(ResponseResult::AgentExplain { explain });
        }
        let Some(agent) = terminal.effective_known_agent().or(terminal.detected_agent) else {
            return failure(
                ApiErrorCode::AgentExplainUnavailable,
                format!(
                    "agent target {} does not have a detected agent label",
                    target.target
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

        success(ResponseResult::AgentExplain { explain: value })
    }
}

fn agent_not_found(target: &str) -> ApiResult {
    Err(ApiError::agent_not_found(target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Mode;
    use crate::test_support::*;
    use shepr_agent::detect::{Agent, AgentState};
    use shepr_api::schema::{AgentStatus, SuccessResponse};
    use shepr_config::Config;
    use shepr_mux::workspace::Workspace;

    fn app_with_agent() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::Test,
            api_rx,
            shepr_api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("agent")];
        app.state.ensure_test_terminals();
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        app.state.mode = Mode::Terminal;
        app
    }

    #[tokio::test]
    async fn a_false_process_exit_makes_a_named_live_agent_unreachable_by_name() {
        // Reproduces a registration loss: a live agent pane with an assigned name stops resolving by that name
        // while its process keeps running, and renaming is the only recovery.
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
        let terminal_id = app.state.workspaces[0].tabs()[0].panes()[&pane_id]
            .attached_terminal_id
            .clone();
        let observed_at = std::time::Instant::now();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition");
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Working);
        terminal.set_agent_name("reviewer".into());

        let found = app.handle_agent_get(&AgentTarget {
            target: "reviewer".into(),
        });
        assert!(
            serde_json::from_str::<SuccessResponse>(&crate::test_support::test_json(&found))
                .is_ok(),
            "the assigned name must resolve while the agent is running: {found:?}"
        );

        // One process-exit observation, then the same agent is observed alive
        // again on the next probe - the process never actually went away.
        app.handle_internal_event(shepr_mux::events::AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            state: AgentState::Idle,
            visible_blocker: false,
            process_exited: true,
            observed_at,
        });
        app.handle_internal_event(shepr_mux::events::AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Pi,
            observed_at: observed_at + std::time::Duration::from_secs(1),
        });

        let terminal = &app.state.terminals[&terminal_id];
        assert_eq!(
            terminal.detected_agent,
            Some(Agent::Pi),
            "the agent process is still there"
        );

        let after = app.handle_agent_get(&AgentTarget {
            target: "reviewer".into(),
        });
        assert!(
            serde_json::from_str::<SuccessResponse>(&crate::test_support::test_json(&after))
                .is_ok(),
            "a live agent must stay reachable by its assigned name: {after:?}"
        );
    }

    #[test]
    fn agent_focus_returns_idle_agent_status() {
        let mut app = app_with_agent();

        let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
        let terminal_id = app.state.workspaces[0].tabs()[0].panes()[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.state.workspaces[0].focus_pane_in_tab(0, pane_id);

        let response = app.handle_agent_focus(&AgentTarget {
            target: app.public_pane_id(0, pane_id).expect("test precondition"),
        });

        let success: SuccessResponse = crate::test_support::test_success(&response);
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info response");
        };
        assert_eq!(agent.agent_status, AgentStatus::Idle);
    }

    #[test]
    fn agent_rename_does_not_replace_the_pane_label() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
        let terminal_id = app.state.workspaces[0].tabs()[0].panes()[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition");
        terminal.set_manual_label("shell-pane".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let target = app.public_pane_id(0, pane_id).expect("test precondition");

        for name in [Some("reviewer".to_string()), None] {
            let response = app.handle_agent_rename(AgentRenameParams {
                target: target.clone(),
                name,
            });
            let success: SuccessResponse = crate::test_support::test_success(&response);
            assert!(matches!(success.result, ResponseResult::AgentInfo { .. }));
            assert_eq!(
                app.state.terminals[&terminal_id].manual_label.as_deref(),
                Some("shell-pane")
            );
        }
    }
}
