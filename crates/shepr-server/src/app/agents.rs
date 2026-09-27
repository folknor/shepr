use shepr_api::error::{ApiError, ApiErrorCode};
use std::time::{Duration, Instant};

use bytes::Bytes;

use super::{App, terminal_targets::TerminalTargetError};
use shepr_api::schema::AgentStartParams;

const DEFAULT_AGENT_START_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_AGENT_START_TIMEOUT: Duration = Duration::from_secs(300);
pub const AGENT_START_SETTLE_DELAY: Duration = Duration::from_secs(3);
const INVALID_AGENT_TIMEOUT_MESSAGE: &str =
    "agent start timeout must be greater than 3000ms and at most 300000ms";
const INVALID_AGENT_NAME_MESSAGE: &str = "agent name must start with a lowercase letter and contain only lowercase letters, digits, '-' or '_' (1-32 characters)";

fn valid_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 32
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_'))
}

impl App {
    pub(super) fn collect_agent_infos(&self) -> Vec<shepr_api::schema::AgentInfo> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.tabs.iter().flat_map(move |tab| {
                    tab.layout
                        .pane_ids()
                        .into_iter()
                        .filter_map(move |pane_id| self.agent_info(ws_idx, pane_id))
                })
            })
            .collect()
    }

    pub(super) fn reconcile_managed_agent_target(&mut self, target: &str) {
        let Ok(resolved) = self.resolve_agent_target(target) else {
            return;
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return;
        };
        let changed = self
            .state
            .terminals
            .get_mut(&terminal_id)
            .is_some_and(|terminal| terminal.reconcile_managed_agent_at(Instant::now(), false));
        if changed {
            self.state.mark_session_dirty();
            self.schedule_session_save();
            self.emit_pane_updated(resolved.ws_idx, resolved.pane_id);
        }
    }

    pub(super) fn agent_info_for_target(
        &self,
        target: &str,
    ) -> Result<shepr_api::schema::AgentInfo, ApiError> {
        self.agent_info_for_target_impl(target)
            .map_err(|err| self.agent_target_error(err))
    }

    fn agent_info_for_target_impl(
        &self,
        target: &str,
    ) -> Result<shepr_api::schema::AgentInfo, TerminalTargetError> {
        let resolved = self.resolve_agent_target(target)?;
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    pub(super) fn focus_agent_target(
        &mut self,
        target: &str,
    ) -> Result<shepr_api::schema::AgentInfo, ApiError> {
        self.focus_agent_target_impl(target)
            .map_err(|err| self.agent_target_error(err))
    }

    fn focus_agent_target_impl(
        &mut self,
        target: &str,
    ) -> Result<shepr_api::schema::AgentInfo, TerminalTargetError> {
        let resolved = self.resolve_agent_target(target)?;
        self.state
            .focus_pane_in_workspace(resolved.ws_idx, resolved.pane_id);
        self.state.mode = crate::app::Mode::Terminal;
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    pub(super) fn rename_agent_target(
        &mut self,
        target: &str,
        name: Option<String>,
    ) -> Result<shepr_api::schema::AgentInfo, ApiError> {
        self.rename_agent_target_impl(target, name)
            .map_err(|err| self.agent_rename_error(err))
    }

    fn rename_agent_target_impl(
        &mut self,
        target: &str,
        name: Option<String>,
    ) -> Result<shepr_api::schema::AgentInfo, AgentRenameError> {
        let resolved = self
            .resolve_agent_target(target)
            .map_err(AgentRenameError::Target)?;
        let normalized_name = match name {
            Some(name) if valid_agent_name(&name) => Some(name),
            Some(_) => return Err(AgentRenameError::InvalidName),
            None => None,
        };

        if let Some(name) = normalized_name.as_deref() {
            let conflicts = self.agent_name_conflicts(name, resolved.terminal_id.as_str());
            if !conflicts.is_empty() {
                return Err(AgentRenameError::DuplicateName {
                    name: name.to_string(),
                    candidates: conflicts,
                });
            }
        }

        let Some(terminal) = self.state.terminals.get_mut(&resolved.terminal_id) else {
            return Err(AgentRenameError::Target(TerminalTargetError::NotFound {
                target: target.to_string(),
            }));
        };
        if terminal.managed_agent_launch_pending() {
            return Err(AgentRenameError::PendingLaunch);
        }
        if terminal.effective_agent_label().is_none() {
            return Err(AgentRenameError::NotAgent);
        }
        match normalized_name {
            Some(name) => terminal.set_agent_name(name),
            None => terminal.clear_agent_name(),
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        self.emit_pane_updated(resolved.ws_idx, resolved.pane_id);
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| {
                AgentRenameError::Target(TerminalTargetError::NotFound {
                    target: target.to_string(),
                })
            })
    }

    pub(super) fn start_agent(
        &mut self,
        params: AgentStartParams,
    ) -> Result<(shepr_api::schema::AgentInfo, Vec<String>), ApiError> {
        self.start_agent_impl(params)
            .map_err(|err| self.agent_start_error(err))
    }

    fn start_agent_impl(
        &mut self,
        params: AgentStartParams,
    ) -> Result<(shepr_api::schema::AgentInfo, Vec<String>), AgentStartError> {
        let name = params.name;
        if !valid_agent_name(&name) {
            return Err(AgentStartError::InvalidName);
        }
        let Some(kind) = shepr_agent::detect::parse_agent_label(&params.kind) else {
            return Err(AgentStartError::UnsupportedKind(params.kind));
        };
        if params
            .args
            .iter()
            .any(|arg| arg.chars().any(char::is_control))
        {
            return Err(AgentStartError::InvalidArgument);
        }
        let persisted_agent_session =
            shepr_agent::agent::resume::persisted_session_from_launch_args(kind, &params.args);
        let conflicts = self.agent_name_conflicts(&name, "");
        if !conflicts.is_empty() {
            return Err(AgentStartError::DuplicateName {
                name,
                candidates: conflicts,
            });
        }
        let Some((ws_idx, pane_id)) = self.parse_current_public_pane_id(&params.pane_id) else {
            return Err(AgentStartError::TargetNotFound(params.pane_id));
        };
        let terminal_id = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
            .cloned()
            .ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        let terminal = self
            .state
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        if terminal.is_agent_terminal() || terminal.managed_agent_kind().is_some() {
            return Err(AgentStartError::TargetBusy(params.pane_id));
        }
        let runtime = self
            .terminal_runtimes
            .get(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
        available_shell_name(runtime)
            .ok_or_else(|| AgentStartError::TargetBusy(params.pane_id.clone()))?;

        let mut argv = vec![shepr_agent::detect::interactive_agent_executable(kind).to_string()];
        argv.extend(params.args);
        let command = shepr_remote::interactive_shell_command(&argv)
            .ok_or(AgentStartError::InvalidArgument)?;
        let bytes = crate::app::api_helpers::encode_api_submission(runtime, &command);
        let timeout =
            Duration::from_millis(params.timeout_ms.unwrap_or(
                u64::try_from(DEFAULT_AGENT_START_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
            ));
        if timeout <= AGENT_START_SETTLE_DELAY || timeout > MAX_AGENT_START_TIMEOUT {
            return Err(AgentStartError::InvalidTimeout);
        }

        let terminal = self
            .state
            .terminals
            .get_mut(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
        // Queue the command before registering the managed launch: a rejected
        // write then leaves the terminal untouched, so there is no rollback to
        // keep in step with everything `begin_managed_agent` sets (name, owner,
        // managed phase, process-acquisition flag). Nothing can observe the
        // pane between the two calls: the write is only queued here, and
        // detection results reach this state through the same main loop.
        runtime
            .try_send_bytes(Bytes::from(bytes))
            .map_err(|err| AgentStartError::InputFailed(err.to_string()))?;
        terminal.begin_managed_agent(
            name.clone(),
            kind,
            Instant::now(),
            AGENT_START_SETTLE_DELAY,
            timeout,
        );
        if let Some(session) = persisted_agent_session {
            terminal.set_managed_agent_launch_session(session);
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();

        let agent = self
            .agent_info(ws_idx, pane_id)
            .ok_or(AgentStartError::TargetUnavailable(params.pane_id))?;
        Ok((agent, argv))
    }

    pub(super) fn agent_start_error(&self, err: AgentStartError) -> ApiError {
        match err {
            AgentStartError::InvalidName => ApiError::new(
                ApiErrorCode::InvalidAgentName,
                INVALID_AGENT_NAME_MESSAGE,
            ),
            AgentStartError::UnsupportedKind(kind) => ApiError::new(
                ApiErrorCode::UnsupportedAgentKind,
                format!("unsupported interactive agent kind {kind}"),
            ),
            AgentStartError::InvalidArgument => ApiError::new(
                ApiErrorCode::InvalidAgentArgument,
                "agent arguments cannot be encoded safely for the target shell",
            ),
            AgentStartError::InvalidTimeout => ApiError::new(
                ApiErrorCode::InvalidAgentTimeout,
                INVALID_AGENT_TIMEOUT_MESSAGE,
            ),
            AgentStartError::TargetNotFound(target) => ApiError::new(
                ApiErrorCode::AgentPaneNotFound,
                format!("agent target pane {target} not found"),
            ),
            AgentStartError::TargetBusy(target) => ApiError::new(
                ApiErrorCode::AgentPaneBusy,
                format!("agent target pane {target} is not an available shell"),
            ),
            AgentStartError::TargetUnavailable(target) => ApiError::new(
                ApiErrorCode::AgentPaneUnavailable,
                format!("agent target pane {target} has no live terminal"),
            ),
            AgentStartError::InputFailed(message) => ApiError::new(
                ApiErrorCode::AgentStartInputFailed,
                message,
            ),
            AgentStartError::DuplicateName { name, candidates } => ApiError::new(
                ApiErrorCode::AgentNameTaken,
                format!(
                    "agent name {name} is already used; candidates: {}",
                    candidates
                        .into_iter()
                        .map(|candidate| format!(
                            "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                            candidate.terminal_id,
                            candidate.pane_id,
                            candidate.workspace_id,
                            candidate.tab_id,
                            candidate.cwd.unwrap_or_else(|| "unknown".into()),
                            candidate.agent_status,
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            ),
        }
    }

    pub(super) fn agent_target_error(&self, err: TerminalTargetError) -> ApiError {
        match err {
            TerminalTargetError::NotFound { target } => ApiError::new(
                ApiErrorCode::AgentNotFound,
                format!("agent target {target} not found"),
            ),
            TerminalTargetError::Ambiguous { target, candidates } => {
                ApiError::new(
                    ApiErrorCode::AgentTargetAmbiguous,
                    format!(
                        "agent target {target} is ambiguous; candidates: {}",
                        candidates
                            .into_iter()
                            .map(|candidate| format!(
                                "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                                candidate.terminal_id,
                                candidate.pane_id,
                                candidate.workspace_id,
                                candidate.tab_id,
                                candidate.cwd.unwrap_or_else(|| "unknown".into()),
                                candidate.agent_status,
                            ))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                )
            }
        }
    }

    pub(super) fn agent_rename_error(&self, err: AgentRenameError) -> ApiError {
        match err {
            AgentRenameError::Target(err) => self.agent_target_error(err),
            AgentRenameError::InvalidName => ApiError::new(
                ApiErrorCode::InvalidAgentName,
                INVALID_AGENT_NAME_MESSAGE,
            ),
            AgentRenameError::NotAgent => ApiError::new(
                ApiErrorCode::AgentNotFound,
                "agent target does not currently host an agent",
            ),
            AgentRenameError::PendingLaunch => ApiError::new(
                ApiErrorCode::AgentLaunchPending,
                "agent name cannot change while startup is pending",
            ),
            AgentRenameError::DuplicateName { name, candidates } => ApiError::new(
                ApiErrorCode::AgentNameTaken,
                format!(
                    "agent name {name} is already used; candidates: {}",
                    candidates
                        .into_iter()
                        .map(|candidate| format!(
                            "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                            candidate.terminal_id,
                            candidate.pane_id,
                            candidate.workspace_id,
                            candidate.tab_id,
                            candidate.cwd.unwrap_or_else(|| "unknown".into()),
                            candidate.agent_status,
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            ),
        }
    }

    pub(super) fn agent_info(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<shepr_api::schema::AgentInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_state = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane_state.attached_terminal_id)?;
        if !terminal.is_agent_terminal() {
            return None;
        }
        let pane = self.pane_info(ws_idx, pane_id)?;
        Some(shepr_api::schema::AgentInfo {
            terminal_id: pane.terminal_id,
            name: terminal.agent_name.clone(),
            agent: pane.agent,
            title: pane.title,
            terminal_title: pane.terminal_title,
            terminal_title_stripped: pane.terminal_title_stripped,
            display_agent: pane.display_agent,
            agent_status: pane.agent_status,
            screen_detection_skipped: terminal.full_lifecycle_hook_authority_active(),
            state_labels: pane.state_labels,
            tokens: pane.tokens,
            agent_session: pane.agent_session,
            workspace_id: pane.workspace_id,
            tab_id: pane.tab_id,
            pane_id: pane.pane_id,
            focused: pane.focused,
            launch_pending: terminal.managed_agent_launch_pending(),
            interactive_ready: terminal.managed_agent_interactive_ready(),
            state_change_seq: terminal.last_agent_state_change_seq.unwrap_or(0),
            cwd: pane.cwd,
            foreground_cwd: pane.foreground_cwd,
            revision: pane.revision,
        })
    }

    fn agent_name_conflicts(
        &self,
        name: &str,
        except_terminal_id: &str,
    ) -> Vec<shepr_api::schema::AgentInfo> {
        self.collect_agent_infos()
            .into_iter()
            .filter(|agent| {
                agent.name.as_deref() == Some(name) && agent.terminal_id != except_terminal_id
            })
            .collect()
    }
}

fn available_shell_name(runtime: &shepr_mux::pane::PaneRuntime) -> Option<String> {
    #[cfg(test)]
    if runtime.child_pid().is_none() {
        return Some("sh".into());
    }
    shepr_agent::detect::available_pane_shell(runtime.child_pid()?)
}

pub(super) fn runtime_hosts_agent(
    runtime: &shepr_mux::pane::PaneRuntime,
    expected: shepr_agent::detect::Agent,
) -> bool {
    #[cfg(test)]
    if runtime.child_pid().is_none() {
        return true;
    }
    live_runtime_agent(runtime) == Some(expected)
}

fn live_runtime_agent(
    runtime: &shepr_mux::pane::PaneRuntime,
) -> Option<shepr_agent::detect::Agent> {
    let job = shepr_agent::detect::foreground_job(runtime.child_pid()?)?;
    shepr_agent::detect::identify_agent_in_job(&job)
        .map(|(agent, _)| agent)
        .or_else(|| {
            job.processes
                .iter()
                .find_map(|process| shepr_agent::detect::process_agent_hint(process.pid))
        })
}

pub(super) enum AgentStartError {
    InvalidName,
    UnsupportedKind(String),
    InvalidArgument,
    InvalidTimeout,
    TargetNotFound(String),
    TargetBusy(String),
    TargetUnavailable(String),
    InputFailed(String),
    DuplicateName {
        name: String,
        candidates: Vec<shepr_api::schema::AgentInfo>,
    },
}

pub(super) enum AgentRenameError {
    Target(TerminalTargetError),
    InvalidName,
    NotAgent,
    PendingLaunch,
    DuplicateName {
        name: String,
        candidates: Vec<shepr_api::schema::AgentInfo>,
    },
}

#[cfg(test)]
mod tests {
    use super::valid_agent_name;

    #[test]
    fn agent_names_use_a_small_cli_safe_grammar() {
        for name in ["a", "reviewer-one", "reviewer_2", &"a".repeat(32)] {
            assert!(valid_agent_name(name), "expected {name:?} to be valid");
        }
        for name in [
            "",
            " reviewer",
            "reviewer ",
            "reviewer one",
            "Reviewer",
            "1reviewer",
            "reviewer.one",
            &"a".repeat(33),
        ] {
            assert!(!valid_agent_name(name), "expected {name:?} to be invalid");
        }
    }
}
