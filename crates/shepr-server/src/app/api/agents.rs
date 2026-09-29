use shepr_api::error::{ApiError, ApiErrorCode, ApiResult};
use std::time::Duration;

use bytes::Bytes;

use crate::app::App;
use shepr_api::schema::{
    AgentPromptParams, AgentRenameParams, AgentSendKeysParams, AgentStartParams, AgentTarget,
    PaneReadResult, ResponseResult,
};
use shepr_pty::actor::{QueuedSubmission, SubmissionCancelOutcome};

use super::responses::{failure, success};

const AGENT_PROMPT_SUBMIT_DELAY: Duration = Duration::from_millis(300);

impl App {
    pub(super) fn handle_agent_list(&mut self) -> ApiResult {
        success(ResponseResult::AgentList {
            agents: self.collect_agent_infos(),
        })
    }

    pub(super) fn handle_agent_get(&mut self, target: &AgentTarget) -> ApiResult {
        self.reconcile_managed_agent_target(&target.target);
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

    pub(super) fn handle_agent_start(&mut self, params: AgentStartParams) -> ApiResult {
        let (agent, argv) = self.start_agent(params)?;

        success(ResponseResult::AgentStarted { agent, argv })
    }

    pub(crate) fn handle_deferred_agent_api_request(
        &mut self,
        request: shepr_api::schema::Request,
        respond_to: std::sync::mpsc::Sender<ApiResult>,
    ) -> bool {
        let shepr_api::schema::Method::AgentPrompt(params) = request.method else {
            return false;
        };
        // `agent.prompt --wait --timeout` sets this. A plain prompt has no
        // caller deadline, so it waits until the PTY accepts the submission.
        let submission_deadline = params
            .wait
            .as_ref()
            .and_then(|wait| wait.submission_deadline);
        let request_id = request.id;
        match self.queue_agent_prompt(&params) {
            Ok((agent, queued)) => {
                std::thread::spawn(move || {
                    let response = match await_prompt_submission(&queued, submission_deadline) {
                        Ok(()) => success(ResponseResult::AgentPrompted { agent }),
                        Err(PromptSubmissionError::TimedOut(outcome)) => {
                            failure(ApiErrorCode::Timeout, prompt_timeout_message(outcome))
                        }
                        Err(PromptSubmissionError::Failed(message)) => {
                            failure(ApiErrorCode::AgentPromptFailed, message)
                        }
                    };
                    shepr_api::send_api_response(
                        &respond_to,
                        &request_id,
                        "agent.prompt",
                        response,
                    );
                });
            }
            Err(response) => {
                shepr_api::send_api_response(
                    &respond_to,
                    &request_id,
                    "agent.prompt",
                    Err(response),
                );
            }
        }
        true
    }

    fn queue_agent_prompt(
        &mut self,
        params: &AgentPromptParams,
    ) -> Result<
        (
            shepr_api::schema::AgentInfo,
            shepr_pty::actor::QueuedSubmission,
        ),
        ApiError,
    > {
        if params.text.is_empty() {
            return Err(ApiError::new(
                ApiErrorCode::EmptyAgentPrompt,
                "agent prompt must not be empty",
            ));
        }
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return Err(self.agent_target_error(err)),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return Err(ApiError::agent_not_found(params.target.clone()));
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return Err(ApiError::agent_not_found(params.target.clone()));
        };
        if terminal.state == shepr_agent::detect::AgentState::Blocked {
            return Err(ApiError::new(
                ApiErrorCode::AgentBlocked,
                format!(
                    "agent {} is blocked and requires interactive input",
                    params.target
                ),
            ));
        }
        let Some(expected_agent) = terminal.effective_known_agent() else {
            return Err(agent_not_ready_error(&params.target));
        };
        if terminal.managed_agent_launch_pending() {
            return Err(agent_not_ready_error(&params.target));
        }
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return Err(ApiError::agent_not_found(params.target.clone()));
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return Err(ApiError::new(
                ApiErrorCode::AgentNotReady,
                format!(
                    "agent {} is no longer the pane foreground process",
                    params.target
                ),
            ));
        }
        if expected_agent == shepr_agent::detect::Agent::GithubCopilot {
            // Copilot ignores synthetic Enter after focus loss until it receives focus gained.
            let focus = shepr_vt::encode_focus(shepr_vt::FocusEvent::Gained);
            if let Err(err) = runtime.try_send_bytes(Bytes::from_static(focus)) {
                return Err(ApiError::new(
                    ApiErrorCode::AgentPromptFailed,
                    err.to_string(),
                ));
            }
        }
        let (text, enter) =
            crate::app::api_helpers::encode_api_submission_parts(runtime, &params.text);
        let Some(agent) = self.agent_info(resolved.ws_idx, resolved.pane_id) else {
            return Err(ApiError::agent_not_found(params.target.clone()));
        };
        let queued = runtime
            .queue_user_input_submission(
                Bytes::from(text),
                Bytes::from(enter),
                AGENT_PROMPT_SUBMIT_DELAY,
            )
            .map_err(|err| ApiError::new(ApiErrorCode::AgentPromptFailed, err.to_string()))?;
        Ok((agent, queued))
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

    pub(super) fn handle_agent_send_keys(&mut self, params: &AgentSendKeysParams) -> ApiResult {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return Err(self.agent_target_error(err)),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(&params.target);
        };
        let Some(expected_agent) = self
            .state
            .terminals
            .get(terminal_id)
            .and_then(shepr_mux::terminal::TerminalState::effective_known_agent)
        else {
            return agent_not_ready(&params.target);
        };
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(&params.target);
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return agent_not_ready(&params.target);
        }
        let encoded = super::super::api_helpers::encode_api_keys(runtime, &params.keys)?;
        let bytes: Vec<u8> = encoded.into_iter().flatten().collect();
        if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
            return failure(ApiErrorCode::AgentSendKeysFailed, err.to_string());
        }

        success(ResponseResult::Ok {})
    }
}

enum PromptSubmissionError {
    /// The caller's deadline passed; carries how far the prompt had got when
    /// it was withdrawn.
    TimedOut(SubmissionCancelOutcome),
    Failed(String),
}

/// Wait for the PTY actor to finish writing a queued prompt. The actor never
/// gives up on its own: an agent that stops reading stdin leaves the write
/// pending forever, so a caller's timeout has to be enforced here. At the
/// deadline the submission is cancelled in the actor, so a prompt the caller
/// was told timed out is not typed and submitted later when the pane starts
/// reading again. Text already partly written is finished (cutting a paste in
/// half would wedge the agent's input), but its Enter is never sent.
fn await_prompt_submission(
    queued: &QueuedSubmission,
    deadline: Option<std::time::Instant>,
) -> Result<(), PromptSubmissionError> {
    let closed = || PromptSubmissionError::Failed("pty actor closed".into());
    let received = match deadline {
        // The PTY completion wait runs on a blocking worker.
        // clock-io-ok: measure remaining real time when it begins waiting.
        Some(deadline) => match queued
            .completion
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            Ok(received) => received,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Err(closed()),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => match queued.cancel.cancel() {
                // The actor finished it as the deadline passed; its reply is
                // sent right after it marks the submission finished.
                SubmissionCancelOutcome::Finished => {
                    queued.completion.recv().map_err(|_| closed())?
                }
                outcome => return Err(PromptSubmissionError::TimedOut(outcome)),
            },
        },
        None => queued.completion.recv().map_err(|_| closed())?,
    };
    received.map_err(|err| PromptSubmissionError::Failed(err.to_string()))
}

fn prompt_timeout_message(outcome: SubmissionCancelOutcome) -> &'static str {
    match outcome {
        SubmissionCancelOutcome::Withdrawn | SubmissionCancelOutcome::Finished => {
            "timed out submitting the agent prompt; the pane is not reading input, and the \
             prompt was withdrawn without typing any of it"
        }
        SubmissionCancelOutcome::TextUnsubmitted => {
            "timed out submitting the agent prompt; the pane is not reading input, and some \
             of the prompt text was already typed into it, but it will not be submitted"
        }
        SubmissionCancelOutcome::AlreadySubmitting => {
            "timed out submitting the agent prompt; the pane is not reading input, and the \
             prompt was already being submitted, so it may still arrive"
        }
    }
}

fn agent_not_ready_error(target: &str) -> ApiError {
    ApiError::new(
        ApiErrorCode::AgentNotReady,
        format!("agent {target} is not an active named agent"),
    )
}

fn agent_not_ready(target: &str) -> ApiResult {
    Err(agent_not_ready_error(target))
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

    fn start_deferred_agent_prompt(
        app: &mut App,
        id: &str,
        params: AgentPromptParams,
    ) -> std::sync::mpsc::Receiver<ApiResult> {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_agent_api_request(
            shepr_api::schema::Request {
                id: id.into(),
                method: shepr_api::schema::Method::AgentPrompt(params),
            },
            respond_to,
        ));
        response_rx
    }

    fn run_deferred_agent_prompt(app: &mut App, id: &str, params: AgentPromptParams) -> String {
        let response = start_deferred_agent_prompt(app, id, params)
            .recv_timeout(Duration::from_secs(1))
            .expect("agent prompt responds after submission");
        shepr_api::error::encode_result(id.into(), response)
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

    #[tokio::test]
    async fn agent_prompt_sends_text_then_delays_enter() {
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
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);
        let (mut runtime, mut rx) =
            shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(80, 24, 0, b"", 2);
        runtime.set_process_probe_result_for_test(None, Some(Agent::OpenCode));
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.insert_test_runtime(pane_id, runtime);

        let public_pane_id = app.public_pane_id(0, pane_id).expect("test precondition");
        let bracketed_started = std::time::Instant::now();
        let response_rx = start_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: public_pane_id,
                text: "A != B".into(),
                wait: None,
            },
        );
        assert!(response_rx.try_recv().is_err());
        let response = response_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("agent prompt responds after submission");
        let success: SuccessResponse = crate::test_support::test_success(&response);
        let ResponseResult::AgentPrompted { agent, .. } = success.result else {
            panic!("expected prompted response");
        };
        assert_eq!(agent.name.as_deref(), Some("reviewer"));
        assert_eq!(
            rx.try_recv().expect("test precondition"),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(
            rx.try_recv().expect("test precondition"),
            Bytes::from_static(b"\r")
        );
        assert!(bracketed_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        app.lookup_runtime_sender(0, pane_id)
            .expect("test precondition")
            .test_process_pty_bytes(b"\x1b[?2004l");
        let raw_started = std::time::Instant::now();
        let raw = run_deferred_agent_prompt(
            &mut app,
            "req-raw",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let raw: SuccessResponse = crate::test_support::test_success(&raw);
        assert!(matches!(raw.result, ResponseResult::AgentPrompted { .. }));
        assert_eq!(
            rx.try_recv().expect("test precondition"),
            Bytes::from_static(b"A != B")
        );
        assert_eq!(
            rx.try_recv().expect("test precondition"),
            Bytes::from_static(b"\r")
        );
        assert!(raw_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        let rejected = run_deferred_agent_prompt(
            &mut app,
            "req-label",
            AgentPromptParams {
                target: "opencode".into(),
                text: "wrong target".into(),
                wait: None,
            },
        );
        let error: shepr_api::schema::ErrorResponse =
            serde_json::from_str(&crate::test_support::test_json(&rejected))
                .expect("test precondition");
        assert_eq!(error.error.code, "agent_not_found");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn prompt_submission_wait_honours_the_callers_deadline() {
        use shepr_pty::actor::SubmissionCancel;
        // A sender that never replies stands in for a pane that stopped
        // reading stdin; at the deadline the submission is withdrawn.
        let (_stalled_tx, stalled) = std::sync::mpsc::channel::<std::io::Result<()>>();
        let stalled = QueuedSubmission {
            completion: stalled,
            cancel: SubmissionCancel::never_started(),
        };
        let started = std::time::Instant::now();
        assert!(matches!(
            await_prompt_submission(&stalled, Some(started + Duration::from_millis(20))),
            Err(PromptSubmissionError::TimedOut(
                SubmissionCancelOutcome::Withdrawn
            ))
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            stalled.cancel.cancel(),
            SubmissionCancelOutcome::Withdrawn,
            "the timed-out prompt was cancelled"
        );

        let (done_tx, done) = std::sync::mpsc::channel();
        done_tx.send(Ok(())).expect("test precondition");
        let done = QueuedSubmission {
            completion: done,
            cancel: SubmissionCancel::never_started(),
        };
        assert!(await_prompt_submission(&done, Some(started)).is_ok());

        let (closed_tx, closed) = std::sync::mpsc::channel::<std::io::Result<()>>();
        drop(closed_tx);
        let closed = QueuedSubmission {
            completion: closed,
            cancel: SubmissionCancel::never_started(),
        };
        assert!(matches!(
            await_prompt_submission(&closed, None),
            Err(PromptSubmissionError::Failed(_))
        ));

        // A submission the actor finished just as the deadline passed
        // reports its real result rather than a timeout.
        let (late_tx, late) = std::sync::mpsc::channel();
        let late = QueuedSubmission {
            completion: late,
            cancel: SubmissionCancel::untracked(),
        };
        let reply = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            late_tx.send(Ok(())).expect("test precondition");
        });
        assert!(await_prompt_submission(&late, Some(started)).is_ok());
        reply.join().expect("test precondition");
    }

    #[tokio::test]
    async fn agent_prompt_rejects_blocked_agent_without_writing() {
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
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Blocked);
        let (runtime, mut rx) = shepr_mux::pane::PaneRuntime::test_with_channel(80, 24);
        app.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "unrelated prompt".into(),
                wait: None,
            },
        );

        let error: shepr_api::schema::ErrorResponse =
            serde_json::from_str(&crate::test_support::test_json(&response))
                .expect("test precondition");
        assert_eq!(error.error.code, "agent_blocked");
        assert!(
            tokio::time::timeout(
                AGENT_PROMPT_SUBMIT_DELAY + Duration::from_millis(100),
                rx.recv()
            )
            .await
            .is_err(),
            "blocked prompt wrote or scheduled terminal input"
        );
    }

    #[tokio::test]
    async fn agent_prompt_focuses_copilot_before_submitting() {
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
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Idle);
        let (mut runtime, mut rx) =
            shepr_mux::pane::PaneRuntime::test_with_channel_and_scrollback_bytes(80, 24, 0, b"", 3);
        runtime.set_process_probe_result_for_test(None, Some(Agent::GithubCopilot));
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let success: SuccessResponse = crate::test_support::test_success(&response);
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(
            rx.try_recv().expect("test precondition"),
            Bytes::from_static(b"\x1b[I")
        );
        assert_eq!(
            rx.try_recv().expect("test precondition"),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(
            rx.try_recv().expect("test precondition"),
            Bytes::from_static(b"\r")
        );
    }

    #[tokio::test]
    async fn agent_send_keys_validates_every_key_before_writing() {
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
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (mut runtime, mut rx) = shepr_mux::pane::PaneRuntime::test_with_channel(80, 24);
        runtime.set_process_probe_result_for_test(None, Some(Agent::Pi));
        app.insert_test_runtime(pane_id, runtime);

        let rejected = app.handle_agent_send_keys(&AgentSendKeysParams {
            target: "reviewer".into(),
            keys: vec!["enter".into(), "not-a-key".into()],
        });
        let error: shepr_api::schema::ErrorResponse =
            serde_json::from_str(&crate::test_support::test_json(&rejected))
                .expect("test precondition");
        assert_eq!(error.error.code, "invalid_key");
        assert!(rx.try_recv().is_err());

        let sent = app.handle_agent_send_keys(&AgentSendKeysParams {
            target: "reviewer".into(),
            keys: vec!["up".into(), "enter".into()],
        });
        let success: SuccessResponse = crate::test_support::test_success(&sent);
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert_eq!(
            rx.try_recv().expect("test precondition"),
            Bytes::from_static(b"\x1b[A\r")
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_managed_agent_while_startup_is_pending() {
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
        let now = std::time::Instant::now();
        terminal.begin_managed_agent(
            "reviewer".into(),
            Agent::OpenCode,
            now,
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(10),
        );
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = shepr_mux::pane::PaneRuntime::test_with_channel(80, 24);
        app.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req-pending",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let error: shepr_api::schema::ErrorResponse =
            serde_json::from_str(&crate::test_support::test_json(&response))
                .expect("test precondition");
        assert_eq!(error.error.code, "agent_not_ready");
        assert!(rx.try_recv().is_err());
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
