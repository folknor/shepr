use std::path::PathBuf;

use super::state::SpawnGeometry;
use super::{App, api_helpers::presented_agent_status};
use shepr_config::NewTerminalCwd;
use shepr_mux::workspace::Workspace;
use shepr_termio::host_term::cell_size::HostCellSize;

pub(crate) fn resolve_new_terminal_cwd(
    policy: &NewTerminalCwd,
    home_dir: Option<&std::path::Path>,
    current_dir: Option<&std::path::Path>,
    follow_cwd: Option<PathBuf>,
) -> PathBuf {
    let fallback = current_dir.unwrap_or_else(|| std::path::Path::new("/"));
    match policy {
        NewTerminalCwd::Follow => follow_cwd
            .or_else(|| home_dir.map(std::path::Path::to_path_buf))
            .unwrap_or_else(|| fallback.to_path_buf()),
        NewTerminalCwd::Home => home_dir.unwrap_or(fallback).to_path_buf(),
        NewTerminalCwd::Current => fallback.to_path_buf(),
        // ServerConfig validation resolved it to an absolute directory at launch
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
    shepr_mux::workspace::terminal_cwd(
        terminal_runtimes.get(terminal_id),
        terminals.get(terminal_id),
        shepr_mux::workspace::CwdPurpose::FollowForNewPane,
    )
}

impl App {
    pub(super) fn seed_cwd_from_workspace(&self, ws_idx: usize) -> Option<PathBuf> {
        let workspace = self.state.workspaces.get(ws_idx)?;
        let root_pane_cwd = workspace.cwd_for_pane(
            workspace.root_pane(),
            &self.state.terminals,
            &self.terminal_runtimes,
        );
        Some(workspace.resolved_identity_cwd_from_root_pane(root_pane_cwd))
    }

    pub(super) fn launch_cwd_for_pane_in_workspace(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<PathBuf> {
        let workspace = self.state.workspaces.get(ws_idx)?;
        launch_cwd_for_terminal(
            workspace.terminal_id(pane_id)?,
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
            Some(self.paths.fallback_cwd()),
            follow_cwd,
        )
    }

    /// Where a new workspace starts when spawned from workspace `ws_idx`: the
    /// focused pane's launch cwd, else the workspace's identity cwd, resolved
    /// through the new-terminal cwd policy.
    pub(crate) fn resolved_new_workspace_cwd(&self, ws_idx: usize) -> PathBuf {
        let follow_cwd = self
            .focused_pane_cwd_in_workspace(ws_idx)
            .or_else(|| self.seed_cwd_from_workspace(ws_idx));
        self.resolve_new_terminal_cwd(follow_cwd)
    }

    /// The geometry a workspace is created in when no client presents one: the
    /// headless area, with no cell size.
    pub(crate) fn headless_spawn_geometry(&self) -> SpawnGeometry {
        SpawnGeometry {
            area: self.state.settings.headless_rect(),
            cell_size: HostCellSize::default(),
        }
    }

    /// Spawns a workspace of one shell pane in `initial_cwd`, sized for
    /// `geometry`: the grid the pane will have there and the pixel size of one
    /// cell, so the shell's first `TIOCSWINSZ` already carries pixel
    /// dimensions. The geometry is recorded for the workspace at once, so a
    /// later split sizes against it before any geometry pass has run. No client
    /// is moved onto the workspace.
    pub(crate) fn create_workspace(
        &mut self,
        initial_cwd: &std::path::Path,
        geometry: SpawnGeometry,
    ) -> std::io::Result<usize> {
        let chrome = self.state.pane_geometry_in(geometry.area);
        let (ws, terminal, runtime) = Workspace::spawn(
            initial_cwd,
            chrome.sole_pane_spawn_geometry(geometry.cell_px()),
            self.state.settings.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.state.host_terminal_appearance,
            shepr_mux::pane::PaneShellConfig::new(
                &self.state.settings.default_shell,
                self.state.settings.login_shell,
            ),
            &self.pane_spawn_handles(),
        )?;
        let terminal_id = terminal.id.clone();
        let outcome = self.state.commit_workspace_creation(ws, terminal);
        self.state
            .record_workspace_geometry(&outcome.workspace_id, geometry);
        self.terminal_runtimes.insert(terminal_id, runtime);
        crate::logging::workspace_created(&outcome.workspace_id, outcome.root_pane.raw());
        Ok(outcome.workspace_index)
    }

    /// The reply a pane command gives the client shell: the pane and its
    /// scroll position. `focused` is left false: whether the pane is focused
    /// depends on which workspace the requesting client views once its command
    /// has run, which only the server loop knows (`fill_reply_focus`). Cheap
    /// on purpose; the per-pane snapshot entry with the `/proc` reads is
    /// `snapshot_pane`.
    pub(super) fn pane_info(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<shepr_protocol::command::PaneInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        if !ws.contains_pane(pane_id) {
            return None;
        }
        let scroll = self
            .state
            .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
            .and_then(shepr_mux::pane::PaneRuntime::scroll_metrics)
            .map(|metrics| shepr_protocol::command::PaneScrollInfo {
                offset_from_bottom: metrics.offset_from_bottom as u64,
                max_offset_from_bottom: metrics.max_offset_from_bottom as u64,
                viewport_rows: metrics.viewport_rows as u64,
            });
        Some(shepr_protocol::command::PaneInfo {
            pane_id: self.public_pane_id(ws_idx, pane_id)?,
            focused: false,
            scroll,
        })
    }

    /// Sets `focused` in a reply's `PaneInfo` or `WorkspaceInfo` for a client
    /// viewing `viewed`: a workspace is focused when it is the viewed one, and a
    /// pane when it is the focused pane of the viewed workspace. Pane focus is
    /// shared by every viewer of a workspace; only which workspace is viewed is
    /// per client.
    pub(crate) fn fill_reply_focus(
        &self,
        reply: &mut shepr_protocol::command::EndpointReply,
        viewed: Option<&shepr_protocol::WorkspaceId>,
    ) {
        use shepr_protocol::command::EndpointReply;
        match reply {
            EndpointReply::PaneInfo { pane } => {
                pane.focused = viewed == Some(pane.pane_id.workspace_id())
                    && self
                        .resolve_pane_id(&pane.pane_id)
                        .is_some_and(|(ws_idx, pane_id)| {
                            self.state
                                .workspaces
                                .get(ws_idx)
                                .is_some_and(|ws| ws.focused_pane_id() == pane_id)
                        });
            }
            EndpointReply::WorkspaceInfo { workspace } => {
                workspace.focused = viewed == Some(&workspace.workspace_id);
            }
            EndpointReply::Done
            | EndpointReply::WorkspaceCheckoutRoot { .. }
            | EndpointReply::PaneSelection { .. }
            | EndpointReply::PaneCopyMotion { .. }
            | EndpointReply::PaneCopySearch { .. }
            | EndpointReply::ClientShellSurfaceSet { .. } => {}
        }
    }

    pub(super) fn lookup_runtime(
        &self,
        ws_idx: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<&shepr_mux::pane::PaneRuntime> {
        self.state
            .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
    }

    /// `focused` is left false, as in `pane_info`; `fill_reply_focus` sets it.
    /// `None` when `index` names no workspace, like `pane_info`: every caller
    /// either resolved the index a moment ago or carries it across an event,
    /// and a stale index must not panic the server.
    pub(super) fn workspace_info(
        &self,
        index: usize,
    ) -> Option<shepr_protocol::command::WorkspaceInfo> {
        let ws = self.state.workspaces.get(index)?;
        let agg_state = ws.aggregate_state(&self.state.terminals);
        Some(shepr_protocol::command::WorkspaceInfo {
            workspace_id: self.public_workspace_id(index)?,
            number: index + 1,
            label: ws.display_name(),
            focused: false,
            pane_count: ws.pane_count(),
            agent_status: presented_agent_status(agg_state),
        })
    }
}
