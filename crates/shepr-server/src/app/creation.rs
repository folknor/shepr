use std::path::PathBuf;

use super::{App, api_helpers::pane_agent_status};
use shepr_config::NewTerminalCwd;
use shepr_mux::workspace::Workspace;

pub(crate) fn resolve_new_terminal_cwd(
    policy: &NewTerminalCwd,
    home_dir: Option<&std::path::Path>,
    current_dir: Option<&std::path::Path>,
    follow_cwd: Option<PathBuf>,
) -> PathBuf {
    match policy {
        NewTerminalCwd::Follow => follow_cwd
            .or_else(|| home_dir.map(std::path::Path::to_path_buf))
            .or_else(|| current_dir.map(std::path::Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("/")),
        NewTerminalCwd::Home => home_dir
            .map(std::path::Path::to_path_buf)
            .or_else(|| current_dir.map(std::path::Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("/")),
        NewTerminalCwd::Current => {
            current_dir.map_or_else(|| PathBuf::from("/"), std::path::Path::to_path_buf)
        }
        // Config validation resolved it to an absolute directory at launch
        // (`~` expanded, relative paths joined to the launch directory).
        NewTerminalCwd::Path(path) => path.clone(),
    }
}

pub(super) fn launch_cwd_for_terminal(
    terminal_id: &shepr_protocol::TerminalId,
    terminals: &std::collections::HashMap<
        shepr_protocol::TerminalId,
        shepr_mux::terminal::TerminalState,
    >,
    terminal_runtimes: &shepr_mux::pane::PaneRuntimeRegistry,
) -> Option<PathBuf> {
    terminal_runtimes
        .get(terminal_id)
        .and_then(shepr_mux::pane::PaneRuntime::follow_cwd)
        .or_else(|| {
            terminals
                .get(terminal_id)
                .map(|terminal| terminal.cwd().to_path_buf())
        })
}

impl App {
    pub(super) fn seed_cwd_from_workspace(&self, ws_idx: usize) -> Option<PathBuf> {
        self.state
            .workspaces
            .get(ws_idx)?
            .resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
    }

    pub(super) fn launch_cwd_for_pane_in_workspace(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<PathBuf> {
        let workspace = self.state.workspaces.get(ws_idx)?;
        let tab = workspace
            .tabs()
            .get(workspace.find_tab_index_for_pane(pane_id)?)?;
        launch_cwd_for_terminal(
            tab.terminal_id(pane_id)?,
            &self.state.terminals,
            &self.terminal_runtimes,
        )
    }

    pub(super) fn focused_pane_cwd_in_workspace(&self, ws_idx: usize) -> Option<PathBuf> {
        let pane_id = self.state.workspaces.get(ws_idx)?.focused_pane_id();
        self.launch_cwd_for_pane_in_workspace(ws_idx, pane_id)
    }

    pub(super) fn resolve_new_terminal_cwd(&self, follow_cwd: Option<PathBuf>) -> PathBuf {
        resolve_new_terminal_cwd(
            &self.state.settings.new_terminal_cwd,
            self.paths.home_dir(),
            self.paths.current_dir(),
            follow_cwd,
        )
    }

    pub(crate) fn resolved_new_workspace_cwd_from_tab(
        &self,
        ws_idx: usize,
        tab_idx: Option<usize>,
    ) -> PathBuf {
        let follow_cwd = tab_idx
            .and_then(|tab_idx| self.state.workspaces.get(ws_idx)?.tabs().get(tab_idx))
            .map(|tab| tab.layout().focused())
            .and_then(|pane_id| self.launch_cwd_for_pane_in_workspace(ws_idx, pane_id))
            .or_else(|| self.seed_cwd_from_workspace(ws_idx));
        self.resolve_new_terminal_cwd(follow_cwd)
    }

    pub(crate) fn create_workspace_with_options(
        &mut self,
        initial_cwd: &std::path::Path,
        focus: bool,
    ) -> std::io::Result<usize> {
        self.create_workspace_with_launch_env(initial_cwd, focus, Vec::new())
    }

    pub(crate) fn create_workspace_with_launch_env(
        &mut self,
        initial_cwd: &std::path::Path,
        focus: bool,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<usize> {
        let (rows, cols) = self.state.pane_geometry().sole_pane_size();
        let (ws, terminal, runtime) = Workspace::new_with_extra_env(
            initial_cwd,
            rows,
            cols,
            self.state.settings.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.state.host_terminal_appearance,
            shepr_mux::pane::PaneShellConfig::new(
                &self.state.settings.default_shell,
                self.state.settings.login_shell,
            ),
            &self.pane_spawn_handles(),
            extra_env,
        )?;
        let terminal_id = terminal.id.clone();
        let outcome = self.state.commit_workspace_creation(ws, terminal, focus);
        self.terminal_runtimes.insert(terminal_id, runtime);
        crate::logging::workspace_created(&outcome.workspace_id, outcome.root_pane.raw());
        self.schedule_session_save();
        Ok(outcome.workspace_index)
    }

    pub(super) fn collect_panes(&self) -> Vec<shepr_api::schema::PaneInfo> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.tabs()
                    .iter()
                    .flat_map(|tab| tab.layout().pane_ids().into_iter())
                    .filter_map(move |pane_id| self.pane_info(ws_idx, pane_id))
            })
            .collect()
    }

    pub(super) fn tab_info(
        &self,
        ws_idx: usize,
        tab_idx: usize,
    ) -> Option<shepr_api::schema::TabInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab = ws.tabs().get(tab_idx)?;
        let agg_state = tab.aggregate_state(&self.state.terminals);
        Some(shepr_api::schema::TabInfo {
            tab_id: self.public_tab_id(ws_idx, tab_idx)?,
            workspace_id: self.public_workspace_id(ws_idx)?,
            number: tab.number(),
            label: ws.tab_display_name(tab_idx)?,
            focused: self.state.active_index() == Some(ws_idx) && ws.active_tab_index() == tab_idx,
            pane_count: tab.panes().len(),
            agent_status: pane_agent_status(agg_state),
        })
    }

    pub(super) fn workspace_created_result(
        &self,
        ws_idx: usize,
    ) -> Option<shepr_api::schema::ResponseResult> {
        Some(shepr_api::schema::ResponseResult::WorkspaceCreated {
            workspace: self.workspace_info(ws_idx)?,
            tab: self.tab_info(ws_idx, 0)?,
            root_pane: self.root_pane_info(ws_idx, 0)?,
        })
    }

    pub(super) fn tab_created_result(
        &self,
        ws_idx: usize,
        tab_idx: usize,
    ) -> Option<shepr_api::schema::ResponseResult> {
        Some(shepr_api::schema::ResponseResult::TabCreated {
            tab: self.tab_info(ws_idx, tab_idx)?,
            root_pane: self.root_pane_info(ws_idx, tab_idx)?,
        })
    }

    pub(super) fn root_pane_info(
        &self,
        ws_idx: usize,
        tab_idx: usize,
    ) -> Option<shepr_api::schema::PaneInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab = ws.tabs().get(tab_idx)?;
        self.pane_info(ws_idx, tab.root_pane())
    }

    pub(super) fn pane_info(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<shepr_api::schema::PaneInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane.attached_terminal_id)?;
        let tab_idx = ws.find_tab_index_for_pane(pane_id)?;
        let scroll = self
            .state
            .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
            .and_then(shepr_mux::pane::PaneRuntime::scroll_metrics)
            .map(|metrics| shepr_api::schema::PaneScrollInfo {
                offset_from_bottom: metrics.offset_from_bottom as u64,
                max_offset_from_bottom: metrics.max_offset_from_bottom as u64,
                viewport_rows: metrics.viewport_rows as u64,
            });
        let focused = self.state.active_index() == Some(ws_idx)
            && ws.active_tab_index() == tab_idx
            && ws.focused_pane_id() == pane_id;
        let tab = ws.tabs().get(tab_idx)?;
        Some(shepr_api::schema::PaneInfo {
            pane_id: self.public_pane_id(ws_idx, pane_id)?,
            terminal_id: terminal.id.clone(),
            workspace_id: self.public_workspace_id(ws_idx)?,
            tab_id: self.public_tab_id(ws_idx, tab_idx)?,
            focused,
            cwd: tab
                .cwd_for_pane(pane_id, &self.state.terminals, &self.terminal_runtimes)
                .map(|cwd| cwd.display().to_string()),
            // Runs on the server main loop once per pane for every
            // `session.snapshot`, so the runtime accessor behind it must stay a
            // few /proc reads and never wait on the PTY actor thread.
            foreground_cwd: tab
                .foreground_cwd_for_pane(pane_id, &self.terminal_runtimes)
                .map(|cwd| cwd.display().to_string()),
            restore_error: terminal.restore_error.as_ref().map(ToString::to_string),
            label: terminal.manual_label.clone(),
            agent: terminal.effective_agent_label().map(str::to_string),
            terminal_title: terminal.terminal_title.clone(),
            terminal_title_stripped: terminal.terminal_title_stripped(),
            agent_status: pane_agent_status(terminal.state),
            agent_session: terminal_agent_session_info(terminal),
            scroll,
            revision: terminal.revision(),
        })
    }

    pub(super) fn lookup_runtime(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<(&shepr_mux::pane::PaneRuntime, shepr_protocol::WorkspaceId)> {
        let runtime =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)?;
        Some((runtime, self.public_workspace_id(ws_idx)?))
    }

    /// `None` when `index` names no workspace, like `tab_info` and
    /// `pane_info`: every caller either resolved the index a moment ago or
    /// carries it across an event, and a stale index must not panic the server.
    pub(super) fn workspace_info(&self, index: usize) -> Option<shepr_api::schema::WorkspaceInfo> {
        let ws = self.state.workspaces.get(index)?;
        let agg_state = ws.aggregate_state(&self.state.terminals);
        Some(shepr_api::schema::WorkspaceInfo {
            workspace_id: self.public_workspace_id(index)?,
            number: index + 1,
            label: ws.display_name(),
            focused: self.state.active_index() == Some(index),
            pane_count: ws.pane_count(),
            tab_count: ws.tabs().len(),
            active_tab_id: self.public_tab_id(index, ws.active_tab_index())?,
            agent_status: pane_agent_status(agg_state),
        })
    }
}

fn terminal_agent_session_info(
    terminal: &shepr_mux::terminal::TerminalState,
) -> Option<shepr_api::schema::AgentSessionInfo> {
    if let Some(authority) = terminal.hook_authority.as_ref()
        && let Some(session_ref) = authority.session_ref.as_ref()
    {
        return Some(shepr_api::schema::AgentSessionInfo {
            source: authority.source.clone(),
            agent: authority.agent_label.clone(),
            kind: session_ref.kind(),
            value: session_ref.value(),
        });
    }

    terminal
        .persisted_agent_session
        .as_ref()
        .map(|session| shepr_api::schema::AgentSessionInfo {
            source: session.source.to_source_string(),
            agent: session.agent.label().to_owned(),
            kind: session.session_ref.kind(),
            value: session.session_ref.value(),
        })
}
