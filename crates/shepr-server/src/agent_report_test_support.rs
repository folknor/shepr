//! A test seam: an [`App`] holding one pane, driven only through the report
//! handlers the API dispatches `pane.report_agent` and
//! `pane.report_agent_session` to.
//!
//! Production never builds one. It is public, and hidden from the docs,
//! because the agent integration contract test lives outside this crate and
//! replays whole hook requests through the real handlers rather than a copy of
//! their dispatch; the library has no test-only feature for that. The `App`
//! stays private to the harness, so a caller can do nothing to it that an API
//! client could not: apply a report request, then read the pane's terminal
//! state.

use std::path::Path;

use shepr_api::error::{ApiError, ApiErrorCode};
use shepr_api::schema::{Method, Request};
use shepr_mux::pane::PaneState;
use shepr_mux::terminal::TerminalState;
use shepr_mux::terminal::state::HookClockSample;
use shepr_mux::workspace::{Workspace, WorkspacePane};

use crate::app::{App, AppClock, AppPolicy};

/// An `App` with one workspace whose single pane has a terminal but no
/// spawned process, which persists nothing.
pub struct AgentReportHarness {
    app: App,
    pane_id: String,
    terminal_id: shepr_protocol::TerminalId,
}

impl AgentReportHarness {
    /// Builds the harness with its data directory and paths below `root`.
    /// The pane starts with `agent` detected as its process at `at`, the
    /// observation that normally precedes an agent's hook reports.
    pub fn new(
        root: &Path,
        agent: shepr_agent::agent::Agent,
        at: HookClockSample,
    ) -> Result<Self, String> {
        let paths = shepr_config::AppPaths::rooted_at(root, Some(root), Some(root));
        let config = shepr_config::ValidatedConfig::from_values(
            shepr_config::Config::default(),
            None,
            paths.clone(),
        )
        .map_err(|errors| errors.join("; "))?;
        let lease = shepr_mux::persist::DataDirLease::acquire(paths.data_dir())
            .map_err(|error| error.to_string())?;
        // No request arrives through the API channel; requests are applied
        // directly with `apply_request`.
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::with_paths(
            &config,
            &paths,
            lease,
            AppPolicy::Suspended,
            api_rx,
            app_clock(at),
        );

        let pane = shepr_core::layout::PaneId::alloc();
        let terminal_id = shepr_protocol::TerminalId::alloc();
        let mut terminal = TerminalState::new(terminal_id.clone(), root.to_path_buf());
        terminal.set_detected_agent_process_at(agent, at.monotonic);
        app.state.terminals.insert(terminal_id.clone(), terminal);
        app.state.workspaces.push(Workspace::test_from_pane(
            Some("agent-report-contract".to_owned()),
            root,
            pane,
            WorkspacePane::new(PaneState::new(terminal_id.clone())),
        ));
        let pane_id = app
            .public_pane_id(0, pane)
            .ok_or_else(|| "test pane has no public id".to_owned())?
            .to_string();
        Ok(Self {
            app,
            pane_id,
            terminal_id,
        })
    }

    /// The public id of the single pane, for the requests' `pane_id`.
    pub fn pane_id(&self) -> &str {
        &self.pane_id
    }

    /// Applies one agent-report request through the handler the API
    /// dispatches it to, with the App's clock at `at`, as the server loop sets
    /// it at the start of each iteration. Any other method is refused.
    pub fn apply_request(&mut self, request: Request, at: HookClockSample) -> Result<(), ApiError> {
        self.app.set_clock(app_clock(at));
        match request.method {
            Method::PaneReportAgent(params) => {
                self.app.handle_pane_report_agent(params).map(|_| ())
            }
            Method::PaneReportAgentSession(params) => self
                .app
                .handle_pane_report_agent_session(params)
                .map(|_| ()),
            method => Err(ApiError::new(
                ApiErrorCode::InvalidRequest,
                format!("expected an agent-report request, got {method:?}"),
            )),
        }
    }

    /// The single pane's terminal state, `None` only if a handler removed it.
    pub fn terminal_state(&self) -> Option<&TerminalState> {
        self.app.state.terminals.get(&self.terminal_id)
    }
}

fn app_clock(at: HookClockSample) -> AppClock {
    AppClock {
        now: at.monotonic,
        wall_now: at.wall,
    }
}
