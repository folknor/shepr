//! Per-client views on the one session.
//!
//! Every connection keeps its own presentation: its location (the focused
//! workspace), its surface size, its outer-terminal focus. Nothing here
//! projects one client's view into `AppState`. What the session has one of is
//! decided from all the views together:
//!
//! - Pane PTY geometry follows the PTY size rule (`workspace_geometry_source`),
//!   applied workspace by workspace, with the area recorded on the session so
//!   spawn sizing and API geometry read the size the panes actually have.
//! - Pane focus reporting follows the focused viewers (`sync_pane_focus`): a
//!   pane holds terminal focus while some client whose outer terminal is
//!   focused views it as its workspace's focused pane.

use super::*;
use crate::app::SpawnGeometry;
use crate::server::ClientId;
use crate::server::clients::{ClientShellLocation, ClientShellTopology};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ShellFocusTarget {
    pub(super) workspace_id: shepr_protocol::WorkspaceId,
    pub(super) pane_id: shepr_core::layout::PaneId,
}

/// Which surface a workspace's PTY geometry comes from
/// (`workspace_geometry_source`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GeometrySource {
    Client(ClientId),
    Headless,
}

/// The PTY size rule; every geometry path goes through it. A pane has one
/// PTY size, so when clients disagree one of them sets it:
///
/// - With exactly one client presenting surfaces, every workspace is sized for
///   it, viewed or not, so switching workspaces never resizes a pane.
/// - Otherwise a workspace is sized for its geometry controller: the client
///   that first viewed it, or that last claimed it by interacting with it
///   (input, outer focus, navigation, surface activation when no other active
///   shell with outer-terminal focus is viewing that workspace). When the
///   controller stops viewing the workspace, a remaining viewer takes it over
///   (`reapply_controlled_shell_workspace_geometry`).
/// - With no client presenting surfaces, every workspace is sized for the
///   configured headless size.
/// - A workspace no client controls while several clients present keeps the
///   size it has: `None`.
fn workspace_geometry_source(
    clients: &crate::server::clients::ClientRegistry,
    workspace_id: &shepr_protocol::WorkspaceId,
) -> Option<GeometrySource> {
    let mut presenting = clients
        .iter()
        .filter(|(_, client)| presents_surface(client))
        .map(|(&client_id, _)| client_id);
    let first = presenting.next();
    if let Some(sole) = first
        && presenting.next().is_none()
    {
        return Some(GeometrySource::Client(sole));
    }
    if let Some(controller) = clients
        .geometry_controller(workspace_id)
        .filter(|controller| {
            clients
                .get(controller)
                .is_some_and(ClientConnection::is_active_shell_client)
        })
    {
        return Some(GeometrySource::Client(controller));
    }
    first.is_none().then_some(GeometrySource::Headless)
}

/// Whether a client presents a surface: an active shell with a way to send
/// frames. Only such a client sizes panes, controls geometry or is chosen to
/// create a workspace.
fn presents_surface(client: &ClientConnection) -> bool {
    client.is_active_shell_client() && client.writer.is_some()
}

impl HeadlessServer {
    /// The workspace `client_id` views: what its own location names, if that
    /// is still a workspace. A client with no workspace views none; nothing
    /// falls back to the session's bookmark.
    pub(super) fn shell_target_for_client(
        &self,
        client_id: ClientId,
    ) -> Option<shepr_protocol::WorkspaceId> {
        self.clients
            .get(&client_id)?
            .shell_state()
            .location
            .focused_workspace_id
            .clone()
            .filter(|workspace_id| self.app.state.workspace_index(workspace_id).is_some())
    }

    /// The location a client that just connected starts at: the session's
    /// bookmark.
    pub(super) fn initial_client_location(&self) -> ClientShellLocation {
        ClientShellLocation::initial(
            self.app
                .state
                .bookmark_index()
                .and_then(|index| Some((self.app.public_workspace_id(index)?, index))),
        )
    }

    fn client_shell_topology(&self) -> ClientShellTopology {
        ClientShellTopology {
            workspace_ids: self.workspace_order(),
            bookmark_index: self.app.state.bookmark_index(),
        }
    }

    /// The session's workspaces in order.
    pub(super) fn workspace_order(&self) -> Vec<shepr_protocol::WorkspaceId> {
        self.app
            .state
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect()
    }

    /// Brings the bookmark, every client location, geometry controller and
    /// recorded workspace geometry in line with the session's workspaces
    /// after they changed. Remembered indices are refreshed on every call, so
    /// an order change is followed by one. Returns whether some client now
    /// views another workspace, which needs a render.
    pub(super) fn reconcile_client_shell_locations(&mut self) -> bool {
        self.app.state.reconcile_bookmark();
        let topology = self.client_shell_topology();
        let live_workspaces = topology
            .workspace_ids
            .iter()
            .collect::<HashSet<&shepr_protocol::WorkspaceId>>();
        let live_clients = self.clients.keys().copied().collect::<HashSet<_>>();
        self.clients
            .retain_geometry_controllers(|workspace_id, client_id| {
                live_workspaces.contains(workspace_id) && live_clients.contains(&client_id)
            });
        self.app.state.retain_live_workspace_geometry();
        let mut changed = false;
        for client in self.clients.values_mut() {
            changed |= client.shell_state_mut().location.reconcile(&topology);
        }
        changed
    }

    /// Applies a command's navigation effect: moves `client_id` onto
    /// `workspace_id`. The session's bookmark follows only the navigation of a
    /// client whose surface is active. Returns whether the client now views
    /// another workspace.
    pub(super) fn navigate_shell_client(
        &mut self,
        client_id: ClientId,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> bool {
        let Some(index) = self.app.state.workspace_index(workspace_id) else {
            return false;
        };
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        let surface_active = client.is_active_shell_client();
        let moved = client
            .shell_state_mut()
            .location
            .navigate(workspace_id.clone(), index);
        if moved {
            crate::logging::workspace_focused(workspace_id);
        }
        if surface_active {
            self.app.state.set_bookmark(workspace_id);
        }
        moved
    }

    fn focus_target_for_surface(
        &self,
        workspace_id: shepr_protocol::WorkspaceId,
    ) -> Option<ShellFocusTarget> {
        let workspace_index = self.app.state.workspace_index(&workspace_id)?;
        let pane_id = self
            .app
            .state
            .workspaces
            .get(workspace_index)?
            .focused_pane_id();
        Some(ShellFocusTarget {
            workspace_id,
            pane_id,
        })
    }

    /// The client an automatically created workspace is sized for and
    /// controlled by: `trigger` if it presents a surface, else the presenting
    /// client with the lowest id. `None` when no client presents one, and the
    /// workspace is sized for the headless area with no controller.
    fn automatic_creation_source(&self, trigger: Option<ClientId>) -> Option<ClientId> {
        trigger
            .filter(|client_id| self.clients.get(client_id).is_some_and(presents_surface))
            .or_else(|| {
                self.clients
                    .iter()
                    .filter(|(_, client)| presents_surface(client))
                    .map(|(&client_id, _)| client_id)
                    .min()
            })
    }

    /// The one function that creates a workspace nobody asked for: the
    /// session's first workspace (a client connecting to an empty server), and
    /// the replacement of the last one when it closed. `trigger` is the client
    /// whose connection or command led to it (the connecting client at setup,
    /// the requester for an endpoint command), and `None` for the main loop and
    /// the JSON paths.
    ///
    /// The workspace is sized for, and controlled by, the client
    /// `automatic_creation_source` picks from the clients as they are now.
    /// The intended controller is assigned before the generic settlement
    /// below, so that settlement does not hand the workspace to someone else.
    /// Every success is followed by a reconcile (which refreshes remembered
    /// indices), the geometry settlement and the pane focus reports. Returns
    /// whether a workspace was created.
    pub(super) fn create_automatic_workspace(&mut self, trigger: Option<ClientId>) -> bool {
        if !self.app.state.workspaces.is_empty() {
            return false;
        }
        // The loop and the JSON paths only replace a workspace for a session
        // some client is looking at.
        if trigger.is_none() && self.clients.latest_shell_client().is_none() {
            return false;
        }
        let source = self.automatic_creation_source(trigger);
        let geometry = source
            .and_then(|client_id| self.client_geometry(client_id))
            .unwrap_or_else(|| self.app.headless_spawn_geometry());
        if !self.app.create_default_workspace(geometry) {
            return false;
        }
        self.immediate_pty_sources_dirty = true;
        if let (Some(client_id), Some(workspace)) = (source, self.app.state.workspaces.last()) {
            self.clients
                .set_geometry_controller(workspace.id.clone(), client_id);
        }
        self.reconcile_client_shell_locations();
        self.reapply_controlled_shell_workspace_geometry(false);
        self.sync_pane_focus();
        true
    }

    /// The pane a client's keyboard reaches: the focused pane of the workspace
    /// its location names.
    pub(super) fn shell_focus_target(&self, client_id: ClientId) -> Option<ShellFocusTarget> {
        self.focus_target_for_surface(self.shell_target_for_client(client_id)?)
    }

    /// The panes that hold terminal focus: the focus target of every active
    /// shell client whose outer terminal reported focus. Several viewers of
    /// one pane focus it once.
    fn panes_holding_focus(
        &self,
    ) -> HashSet<(shepr_protocol::WorkspaceId, shepr_core::layout::PaneId)> {
        self.clients
            .iter()
            .filter(|(_, client)| {
                client.is_active_shell_client()
                    && client.shell_state().outer_terminal_focus == Some(true)
            })
            .filter_map(|(&client_id, _)| self.shell_focus_target(client_id))
            .map(|target| (target.workspace_id, target.pane_id))
            .collect()
    }

    /// Reports focus changes to the panes (`CSI I` / `CSI O` for panes that
    /// enabled focus reporting): every pane that stopped being focused loses
    /// focus and every newly focused pane gains it. Level-based, so it is
    /// correct whatever changed the views (navigation, a client leaving, a
    /// pane dying, an outer focus report) and idempotent; the handlers call
    /// it once their change is applied.
    pub(super) fn sync_pane_focus(&mut self) {
        let focused = self.panes_holding_focus();
        // A pane whose runtime was replaced (an agent resume starting its
        // shell) stays in the set, so the set diff never tells the new
        // runtime; it gets the focus-in report here. A replaced pane that
        // newly gains focus is told by the diff below.
        let replaced = std::mem::take(&mut self.app.runtimes_replaced_panes);
        for pane_id in replaced {
            let Some(workspace_id) = self
                .app
                .find_pane(pane_id)
                .and_then(|(ws_idx, _)| self.app.public_workspace_id(ws_idx))
            else {
                continue;
            };
            let key = (workspace_id, pane_id);
            if focused.contains(&key) && self.focused_panes.contains(&key) {
                self.send_pane_focus(&key.0, pane_id, shepr_vt::FocusEvent::Gained);
            }
        }
        if focused == self.focused_panes {
            return;
        }
        for (workspace_id, pane_id) in self.focused_panes.difference(&focused) {
            self.send_pane_focus(workspace_id, *pane_id, shepr_vt::FocusEvent::Lost);
        }
        for (workspace_id, pane_id) in focused.difference(&self.focused_panes) {
            self.send_pane_focus(workspace_id, *pane_id, shepr_vt::FocusEvent::Gained);
        }
        self.focused_panes = focused;
    }

    fn send_pane_focus(
        &self,
        workspace_id: &shepr_protocol::WorkspaceId,
        pane_id: shepr_core::layout::PaneId,
        event: shepr_vt::FocusEvent,
    ) {
        if let Some(workspace_index) = self
            .app
            .state
            .workspaces
            .iter()
            .position(|workspace| &workspace.id == workspace_id)
        {
            self.app
                .send_pane_focus_event(workspace_index, pane_id, event);
        }
    }

    /// The active shell clients with a writer that view `pane_id`, in id
    /// order: the recipients of a clipboard write from that pane.
    pub(super) fn clipboard_viewers(&self, pane_id: shepr_core::layout::PaneId) -> Vec<ClientId> {
        let Some((workspace_index, _)) = self.app.find_pane(pane_id) else {
            return Vec::new();
        };
        let mut viewers: Vec<ClientId> = self
            .clients
            .iter()
            .filter(|(_, client)| client.is_active_shell_client() && client.writer.is_some())
            .map(|(&client_id, _)| client_id)
            .filter(|&client_id| self.shell_client_views_pane(client_id, workspace_index, pane_id))
            .collect();
        viewers.sort_unstable();
        viewers
    }

    pub(super) fn shell_client_views_pane(
        &self,
        client_id: ClientId,
        workspace_index: usize,
        pane_id: shepr_core::layout::PaneId,
    ) -> bool {
        let Some(target) = self.shell_target_for_client(client_id) else {
            return false;
        };
        if self.app.state.workspace_index(&target) != Some(workspace_index) {
            return false;
        }
        self.app
            .state
            .workspaces
            .get(workspace_index)
            .is_some_and(|workspace| workspace.shows_pane(pane_id))
    }

    pub(super) fn workspace_geometry_source(
        &self,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> Option<GeometrySource> {
        workspace_geometry_source(&self.clients, workspace_id)
    }

    /// The geometry `client_id` presents: its surface size and cell size.
    pub(super) fn client_geometry(&self, client_id: ClientId) -> Option<SpawnGeometry> {
        let client = self.clients.get(&client_id)?;
        Some(SpawnGeometry {
            area: Rect::new(
                0,
                0,
                client.terminal_size.cols.get(),
                client.terminal_size.rows.get(),
            ),
            cell_size: client.cell_size.or_default(),
        })
    }

    /// The geometry a workspace's PTYs are sized for, per the PTY size rule;
    /// `None` when the workspace keeps the size it has.
    fn workspace_geometry(
        &self,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> Option<SpawnGeometry> {
        match self.workspace_geometry_source(workspace_id)? {
            GeometrySource::Client(client_id) => self.client_geometry(client_id),
            GeometrySource::Headless => Some(self.app.headless_spawn_geometry()),
        }
    }

    /// Applies the PTY size rule to one workspace: resizes its visible panes
    /// and records the geometry on the session. Returns whether the rule
    /// sized it.
    pub(super) fn apply_workspace_geometry(
        &mut self,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> bool {
        let Some(workspace_index) = self.app.state.workspace_index(workspace_id) else {
            return false;
        };
        let Some(geometry) = self.workspace_geometry(workspace_id) else {
            return false;
        };
        crate::ui::resize_surface(
            &self.app.state,
            &crate::ui::PaneResizer::new(&self.app.terminal_runtimes),
            workspace_index,
            geometry.area,
            geometry.cell_size,
        );
        self.app
            .state
            .record_workspace_geometry(workspace_id, geometry);
        true
    }

    /// Applies the PTY size rule to every workspace. A pane already at its
    /// size is not resized again, so this is safe to run after any change the
    /// rule depends on.
    pub(super) fn apply_all_workspace_geometry(&mut self) -> bool {
        let workspace_ids: Vec<_> = self
            .app
            .state
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect();
        let mut applied = false;
        for workspace_id in &workspace_ids {
            applied |= self.apply_workspace_geometry(workspace_id);
        }
        applied
    }

    fn finish_shell_workspace_geometry_change(&mut self, start_pending_agent_resumes: bool) {
        for client in self.clients.values_mut() {
            client.request_recompute();
        }
        if !start_pending_agent_resumes {
            return;
        }
        let now = self.app.clock.now;
        if self.app.start_pending_agent_resumes(now) {
            for client in self.clients.values_mut() {
                client.request_recompute();
            }
            self.sync_pane_focus();
        }
    }

    /// Applies the PTY size rule to every workspace and, when it sized any,
    /// has every client recompute its surface and settles pending resumes.
    fn apply_shell_geometry(&mut self, start_pending_agent_resumes: bool) -> bool {
        if !self.apply_all_workspace_geometry() {
            return false;
        }
        self.finish_shell_workspace_geometry_change(start_pending_agent_resumes);
        true
    }

    /// Whether the PTY size rule sizes some workspace for `client_id`.
    fn is_geometry_source(&self, client_id: ClientId) -> bool {
        self.app.state.workspaces.iter().any(|workspace| {
            self.workspace_geometry_source(&workspace.id) == Some(GeometrySource::Client(client_id))
        })
    }

    /// Hands each viewed workspace whose controller no longer views it to one
    /// of its viewers, then applies the PTY size rule to every workspace.
    pub(super) fn reapply_controlled_shell_workspace_geometry(
        &mut self,
        start_pending_agent_resumes: bool,
    ) -> bool {
        let mut viewed_workspaces = HashMap::<shepr_protocol::WorkspaceId, Vec<ClientId>>::new();
        for (&client_id, client) in &self.clients {
            if !client.is_active_shell_client() || client.writer.is_none() {
                continue;
            }
            let Some(workspace_id) = self.shell_target_for_client(client_id) else {
                continue;
            };
            viewed_workspaces
                .entry(workspace_id)
                .or_default()
                .push(client_id);
        }
        for viewers in viewed_workspaces.values_mut() {
            viewers.sort_unstable();
        }
        for (workspace_id, viewers) in viewed_workspaces {
            let controller_is_viewing = self
                .clients
                .geometry_controller(&workspace_id)
                .as_ref()
                .is_some_and(|controller| viewers.contains(controller));
            if !controller_is_viewing {
                self.clients
                    .set_geometry_controller(workspace_id, viewers[0]);
            }
        }
        self.apply_shell_geometry(start_pending_agent_resumes)
    }

    /// Makes `client_id` the geometry controller of the workspace it views
    /// and, if that changed the controller, applies the PTY size rule.
    pub(super) fn claim_shell_workspace_geometry(
        &mut self,
        client_id: ClientId,
        start_pending_agent_resumes: bool,
    ) -> bool {
        if !self
            .clients
            .get(&client_id)
            .is_some_and(ClientConnection::is_active_shell_client)
        {
            return false;
        }
        let Some(workspace_id) = self.shell_target_for_client(client_id) else {
            return false;
        };
        if !self.clients.claim_geometry(workspace_id, client_id) {
            return false;
        }
        self.apply_shell_geometry(start_pending_agent_resumes)
    }

    /// As `claim_shell_workspace_geometry`, for a workspace no client controls
    /// yet.
    pub(super) fn claim_unowned_shell_workspace_geometry(
        &mut self,
        client_id: ClientId,
        start_pending_agent_resumes: bool,
    ) -> bool {
        if !self
            .clients
            .get(&client_id)
            .is_some_and(ClientConnection::is_active_shell_client)
        {
            return false;
        }
        let Some(workspace_id) = self.shell_target_for_client(client_id) else {
            return false;
        };
        if !self.clients.claim_unowned_geometry(workspace_id, client_id) {
            return false;
        }
        self.apply_shell_geometry(start_pending_agent_resumes)
    }

    /// Re-applies the PTY size rule after `client_id`'s own geometry changed
    /// (a resize, a new cell size), when some workspace is sized for it.
    pub(super) fn resize_shell_workspaces_sized_for(
        &mut self,
        client_id: ClientId,
        start_pending_agent_resumes: bool,
    ) -> bool {
        if !self.is_geometry_source(client_id) {
            return false;
        }
        self.apply_shell_geometry(start_pending_agent_resumes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(active: bool, writer: bool) -> ClientConnection {
        let writer = writer.then(|| crate::server::client_transport::ClientWriter::test_pair().0);
        let mut client = ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            writer,
        );
        client.shell_state_mut().surface_active = active;
        client
    }

    #[test]
    fn pty_size_rule_prefers_the_sole_surface_then_the_controller_then_headless() {
        let workspace_id: shepr_protocol::WorkspaceId = shepr_test_fixtures::id("w1");
        let mut clients = crate::server::clients::ClientRegistry::default();
        assert_eq!(
            workspace_geometry_source(&clients, &workspace_id),
            Some(GeometrySource::Headless)
        );

        let first = clients.allocate_client_id();
        clients.insert(first, client(true, true));
        assert_eq!(
            workspace_geometry_source(&clients, &workspace_id),
            Some(GeometrySource::Client(first))
        );

        // A second presenting surface: an uncontrolled workspace keeps its size.
        let second = clients.allocate_client_id();
        clients.insert(second, client(true, true));
        assert_eq!(workspace_geometry_source(&clients, &workspace_id), None);
        assert!(clients.claim_geometry(workspace_id.clone(), second));
        assert_eq!(
            workspace_geometry_source(&clients, &workspace_id),
            Some(GeometrySource::Client(second))
        );

        // An inactive surface presents nothing: the other one is sole again.
        if let Some(client) = clients.get_mut(&second) {
            client.shell_state_mut().surface_active = false;
        }
        assert_eq!(
            workspace_geometry_source(&clients, &workspace_id),
            Some(GeometrySource::Client(first))
        );

        // A metadata-only connection never sizes anything.
        let (_, _) = clients.remove_client(first);
        assert_eq!(
            workspace_geometry_source(&clients, &workspace_id),
            Some(GeometrySource::Headless)
        );
    }
}
