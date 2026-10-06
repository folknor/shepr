//! A test seam: an [`App`] holding one pane, driven only through the report
//! handlers the API dispatches `pane.report_agent` and
//! `pane.report_agent_session` to.
//!
//! Test-only. The agent integration contract test replays whole hook requests
//! through the real handlers rather than a copy of their dispatch. The `App`
//! stays private to the harness, so a caller can do nothing to it that an API
//! client could not: apply a report request, then read the pane's terminal
//! state.

// This harness intentionally exercises App's real API dispatch. Ownership's
// pure transition tests live in shepr-agent; this seam covers server wiring.

use std::path::Path;

use shepr_api::error::{ApiError, ApiErrorCode};
use shepr_api::schema::{Method, Request};
use shepr_detect::ownership::HookClockSample;
use shepr_mux::terminal::TerminalState;
use shepr_mux::workspace::Workspace;

use crate::app::{App, AppClock, AppOutputs};
use crate::test_support::ValidatedServerConfigFixture as _;

/// An `App` with one workspace whose single pane has a terminal but no
/// spawned process. Its persister owns the data directory under the root, but
/// no event loop runs to start a save.
pub(crate) struct AgentReportHarness {
    app: App,
    /// Held so the app's event channel stays open; nothing waits on it.
    _outputs: AppOutputs,
    pane_id: String,
    pane: shepr_core::layout::PaneId,
}

impl AgentReportHarness {
    /// Builds the harness with its data directory and paths below `root`.
    /// The pane starts with `agent` detected as its process at `at`, the
    /// observation that normally precedes an agent's hook reports.
    pub(crate) fn new(
        root: &Path,
        agent: shepr_agent::Agent,
        at: HookClockSample,
    ) -> Result<Self, String> {
        let paths = shepr_paths::AppPaths::rooted_at(root, Some(root), Some(root))
            .map_err(|error| error.to_string())?;
        // This harness does not spawn a shell. The config fixture gives
        // validation a fixed stand-in instead of resolving the test runner's
        // inherited shell and PATH.
        let config = shepr_config::ValidatedServerConfig::test_from_config_with_paths(
            shepr_config::ServerConfig::default(),
            paths.clone(),
        );
        let lease = shepr_mux::persist::DataDirLease::acquire(paths.data_dir())
            .map_err(|error| error.to_string())?;
        // Requests are applied directly with `apply_request`.
        let (mut app, outputs) = App::open(&config, &paths, lease, app_clock(at));

        let pane = shepr_core::layout::PaneId::alloc();
        let root_path = shepr_core::absolute_path::AbsolutePath::new(root)
            .map_err(|_| "test root is not absolute".to_owned())?;
        let mut terminal = TerminalState::new(root_path.clone());
        terminal
            .ownership_mut()
            .set_detected_agent_process_at(agent, at.monotonic);
        app.test_state_mut()
            .test_push_workspace(Workspace::test_from_pane(
                crate::test_support::next_fixture_workspace_id(),
                Some("agent-report-contract".to_owned()),
                &root_path,
                pane,
                terminal,
            ));
        let pane_id = app
            .state()
            .pane(pane)
            .ok_or_else(|| "test pane has no public id".to_owned())?
            .public_id()
            .to_string();
        Ok(Self {
            app,
            _outputs: outputs,
            pane_id,
            pane,
        })
    }

    /// The public id of the single pane, for the requests' `pane_id`.
    pub(crate) fn pane_id(&self) -> &str {
        &self.pane_id
    }

    /// Applies one agent-report request through the handler the API
    /// dispatches it to, with the App's clock at `at`, as the server loop sets
    /// it at the start of each iteration. Any other method is refused.
    pub(crate) fn apply_request(
        &mut self,
        request: Request,
        at: HookClockSample,
    ) -> Result<(), ApiError> {
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
    pub(crate) fn terminal_state(&self) -> Option<&TerminalState> {
        self.app.state().terminal(self.pane)
    }
}

fn app_clock(at: HookClockSample) -> AppClock {
    AppClock::from(at)
}
