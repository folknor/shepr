use shepr_core::absolute_path::AbsolutePath;

use super::{App, SpawnGeometry};
use shepr_config::NewTerminalCwd;

/// `home_dir` and `fallback_cwd` are the launch's `AppPaths`
/// (`AppPaths::fallback_cwd` is the launch directory, else the root).
pub(crate) fn resolve_new_terminal_cwd(
    policy: &NewTerminalCwd,
    home_dir: Option<&AbsolutePath>,
    fallback_cwd: &AbsolutePath,
    follow_cwd: Option<AbsolutePath>,
) -> AbsolutePath {
    let home = home_dir.cloned();
    let fallback = || fallback_cwd.clone();
    match policy {
        NewTerminalCwd::Follow => follow_cwd.or(home).unwrap_or_else(fallback),
        NewTerminalCwd::Home => home.unwrap_or_else(fallback),
        NewTerminalCwd::Current => fallback(),
        // ServerConfig validation resolved it to an absolute directory at launch
        // (`~` expanded, relative paths joined to the launch directory).
        NewTerminalCwd::Path(path) => path.clone(),
    }
}

impl App {
    pub(super) fn seed_cwd_from_workspace(
        &self,
        id: &shepr_protocol::WorkspaceId,
    ) -> Option<AbsolutePath> {
        let workspace = self.state.workspace(id)?;
        Some(workspace.resolved_identity_cwd(&self.terminal_runtimes))
    }

    pub(super) fn launch_cwd_for_pane(
        &self,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<AbsolutePath> {
        let terminal = self.state.terminal(pane_id)?;
        shepr_mux::workspace::terminal_cwd(
            self.terminal_runtimes.get(&pane_id),
            Some(terminal),
            shepr_mux::workspace::CwdPurpose::FollowForNewPane,
        )
    }

    pub(super) fn focused_pane_cwd_in_workspace(
        &self,
        id: &shepr_protocol::WorkspaceId,
    ) -> Option<AbsolutePath> {
        let pane_id = self.state.workspace(id)?.tree().focused();
        self.launch_cwd_for_pane(pane_id)
    }

    pub(super) fn resolve_new_terminal_cwd(
        &self,
        follow_cwd: Option<AbsolutePath>,
    ) -> AbsolutePath {
        resolve_new_terminal_cwd(
            &self.state.settings().new_terminal_cwd,
            self.paths.home_dir(),
            self.paths.fallback_cwd(),
            follow_cwd,
        )
    }

    /// Where a new workspace starts when spawned from workspace `id`: the
    /// focused pane's launch cwd, else the workspace's identity cwd, resolved
    /// through the new-terminal cwd policy.
    pub(crate) fn resolved_new_workspace_cwd(
        &self,
        id: &shepr_protocol::WorkspaceId,
    ) -> AbsolutePath {
        let follow_cwd = self
            .focused_pane_cwd_in_workspace(id)
            .or_else(|| self.seed_cwd_from_workspace(id));
        self.resolve_new_terminal_cwd(follow_cwd)
    }

    /// The geometry a workspace is created in when no client presents one: the
    /// headless area, with no cell size.
    pub(crate) fn headless_spawn_geometry(&self) -> SpawnGeometry {
        SpawnGeometry {
            area: self.state.settings().headless_rect(),
            cell: None,
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
        initial_cwd: &AbsolutePath,
        geometry: SpawnGeometry,
    ) -> std::io::Result<shepr_protocol::WorkspaceId> {
        self.create_workspace_outcome(initial_cwd, geometry)
            .map(|outcome| outcome.workspace_id)
    }

    pub(crate) fn create_workspace_outcome(
        &mut self,
        initial_cwd: &AbsolutePath,
        geometry: SpawnGeometry,
    ) -> std::io::Result<super::actions::WorkspaceCreationOutcome> {
        let chrome = self.state.settings().chrome_in(geometry.area);
        let prepared = self
            .state
            .prepare_workspace(initial_cwd)
            .ok_or_else(|| std::io::Error::other("workspace ID space exhausted"))?;
        let public_id = prepared.root_public_id();
        let runtime = self.launch_pane(
            prepared.root_pane(),
            prepared.root_public_id(),
            chrome.sole_pane_spawn_geometry(geometry.cell_px()),
            initial_cwd,
            shepr_mux::pane::LaunchKind::Fresh,
        )?;
        let Some(outcome) = self.state.commit_workspace_creation(prepared, geometry) else {
            drop(runtime);
            return Err(std::io::Error::other(
                "the new workspace was refused by the session",
            ));
        };
        self.install_runtime(outcome.root_pane, runtime);
        crate::logging::workspace_created(
            &outcome.workspace_id,
            public_id,
            initial_cwd,
            self.state
                .workspace(&outcome.workspace_id)
                .map_or("", |ws| ws.name()),
        );
        Ok(outcome)
    }

    /// The reply acknowledges the pane target and includes its scroll position.
    /// Focus belongs to the requester-specific shell snapshot; the per-pane
    /// snapshot entry with the `/proc` reads is `snapshot_pane`.
    pub(super) fn pane_info(
        &self,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<shepr_protocol::command::PaneInfo> {
        let pane = self.state.pane(pane_id)?;
        let scroll = self
            .terminal_runtimes
            .get(&pane_id)
            .and_then(|runtime| runtime.read().scroll_metrics());
        Some(shepr_protocol::command::PaneInfo {
            pane_id: pane.public_id(),
            scroll,
        })
    }

    pub(super) fn lookup_runtime(
        &self,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<&shepr_mux::pane::PaneRuntime> {
        self.terminal_runtimes.get(&pane_id)
    }

    /// `None` when `id` names no workspace, like `pane_info`: a caller may
    /// carry the id across an event, and a closed workspace must not panic the
    /// server.
    pub(super) fn workspace_info(
        &self,
        id: &shepr_protocol::WorkspaceId,
    ) -> Option<shepr_protocol::command::WorkspaceInfo> {
        let ws = self.state.workspace(id)?;
        let agg_state = ws.aggregate_state();
        Some(shepr_protocol::command::WorkspaceInfo {
            workspace_id: ws.id(),
            label: ws.name().to_owned(),
            pane_count: ws.tree().len(),
            agent_status: agg_state,
        })
    }
}
