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
//!
//! These stay `HeadlessServer` methods rather than a client-view component:
//! the size rule itself is a free function over `ClientRegistry`, but applying
//! it resizes PTYs, starts pending agent resumes and sends pane focus reports,
//! all through `App`.

use super::*;
use crate::app::{DefaultWorkspace, SpawnGeometry};
use crate::server::ClientId;
use crate::server::clients::{ClientShellLocation, ClientShellTopology};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ShellFocusTarget {
    pub(super) workspace_id: shepr_protocol::WorkspaceId,
    pub(super) pane_id: shepr_core::layout::PaneId,
}

/// A live workspace selected by a client location.
pub(super) struct ViewedWorkspace<'a> {
    pub(super) workspace: &'a shepr_mux::workspace::Workspace,
    /// The workspace's position in the session, as of this resolution.
    index: usize,
}

impl<'a> ViewedWorkspace<'a> {
    pub(super) fn resolve(app: &'a app::App, id: &shepr_protocol::WorkspaceId) -> Option<Self> {
        let workspaces = app.state().workspaces();
        let index = workspaces.position(id)?;
        Some(Self {
            workspace: workspaces.as_slice().get(index)?,
            index,
        })
    }

    /// The view `target` names, if it is still current: the workspace at its
    /// position must still have its id. O(1), for a target resolved earlier in
    /// the same pass.
    pub(super) fn at(app: &'a app::App, target: crate::ui::SurfaceTarget) -> Option<Self> {
        Some(Self {
            workspace: target.workspace(app.state())?,
            index: target.index,
        })
    }

    /// This view as the target the render path carries.
    pub(super) fn target(&self) -> crate::ui::SurfaceTarget {
        crate::ui::SurfaceTarget {
            index: self.index,
            id: self.workspace.id(),
        }
    }

    pub(super) fn for_location(app: &'a app::App, location: &ClientShellLocation) -> Option<Self> {
        Self::resolve(app, location.focused_workspace_id()?)
    }
}

/// Why a client can take ownership of its viewed workspace's geometry.
#[derive(Clone, Copy)]
pub(super) enum GeometryClaimReason {
    Connect,
    Activate,
    Focus,
    Interaction,
    Command {
        claims: bool,
        topology: bool,
        navigated: bool,
    },
}

/// Whether a geometry application also starts the agent resumes that were
/// waiting for a settled geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PendingResumes {
    /// Leave them waiting for a later application.
    Defer,
    /// Start them now.
    Start,
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
/// - Otherwise a workspace is sized for a current viewer. Its last geometry
///   controller wins while it still views that workspace; if it does not, the
///   lowest-id outer-focused viewer wins, then the lowest-id current viewer.
///   With several clients presenting and no viewer, the workspace keeps its
///   size: `None`.
/// - With no client presenting surfaces, every workspace is sized for the
///   configured headless size.
fn workspace_geometry_source(
    clients: &crate::server::clients::ClientRegistry,
    workspace_id: &shepr_protocol::WorkspaceId,
) -> Option<GeometrySource> {
    let last_controller = clients.geometry_controller(workspace_id);
    let mut sole_presenter = None;
    let mut has_multiple_presenters = false;
    let mut lowest_viewer = None;
    let mut lowest_focused_viewer = None;
    let mut current_controller = None;
    for (&client_id, client) in clients.presenting() {
        if sole_presenter.is_some() {
            has_multiple_presenters = true;
        } else {
            sole_presenter = Some(client_id);
        }
        if client.shell_state().location.focused_workspace_id() == Some(workspace_id) {
            if lowest_viewer.is_none_or(|viewer| client_id < viewer) {
                lowest_viewer = Some(client_id);
            }
            if client.shell_state().outer_terminal_focus.is_focused()
                && lowest_focused_viewer.is_none_or(|viewer| client_id < viewer)
            {
                lowest_focused_viewer = Some(client_id);
            }
            if last_controller == Some(client_id) {
                // A remembered choice has effect only while its client is a
                // current viewer; navigation can make it stale before any
                // geometry settlement runs.
                current_controller = Some(client_id);
            }
        }
    }
    if !has_multiple_presenters {
        if let Some(sole) = sole_presenter {
            return Some(GeometrySource::Client(sole));
        }
        return Some(GeometrySource::Headless);
    }
    if let Some(controller) = current_controller {
        return Some(GeometrySource::Client(controller));
    }
    if let Some(viewer) = lowest_focused_viewer.or(lowest_viewer) {
        return Some(GeometrySource::Client(viewer));
    }
    None
}

impl HeadlessServer {
    /// The workspace `client_id` views: what its own location names, if that
    /// is still a workspace. A client with no workspace views none; nothing
    /// falls back to the session's bookmark.
    pub(super) fn shell_target_for_client(
        &self,
        client_id: ClientId,
    ) -> Option<shepr_protocol::WorkspaceId> {
        self.viewed_workspace_for_client(client_id)
            .map(|view| view.workspace.id())
    }

    pub(super) fn viewed_workspace_for_client(
        &self,
        client_id: ClientId,
    ) -> Option<ViewedWorkspace<'_>> {
        ViewedWorkspace::for_location(
            &self.app,
            &self.clients.get(&client_id)?.shell_state().location,
        )
    }

    /// The location a client that just connected starts at: the session's
    /// bookmark.
    pub(super) fn initial_client_location(&self) -> ClientShellLocation {
        let state = self.app.state();
        ClientShellLocation::initial(state.workspaces().bookmark().zip(state.bookmark_index()))
    }

    fn client_shell_topology(&self) -> ClientShellTopology {
        ClientShellTopology {
            workspace_ids: self.workspace_order(),
            bookmark_index: self.app.state().bookmark_index(),
        }
    }

    /// The session's workspaces in order.
    pub(super) fn workspace_order(&self) -> Vec<shepr_protocol::WorkspaceId> {
        self.app
            .state()
            .workspaces()
            .iter()
            .map(shepr_mux::workspace::Workspace::id)
            .collect()
    }

    /// Brings every client location and geometry controller in line with the
    /// session's workspaces after they changed (the bookmark and the recorded
    /// workspace geometry are repaired by the workspace set itself).
    /// Remembered indices are refreshed on every call, so an order change is
    /// followed by one. Returns whether some client now views another
    /// workspace, which needs a render.
    pub(super) fn reconcile_client_shell_locations(&mut self) -> bool {
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
        let mut changed = false;
        for client in self.clients.values_mut() {
            changed |= client.shell_state_mut().location.reconcile(&topology);
        }
        if changed {
            self.refresh_client_view_keys();
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
        let Some(index) = self.app.state().workspaces().position(workspace_id) else {
            return false;
        };
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        let surface_active = client.is_active_shell_client();
        let moved = client
            .shell_state_mut()
            .location
            .navigate(*workspace_id, index);
        if moved {
            crate::logging::workspace_focused(workspace_id);
            self.refresh_client_view_keys();
        }
        if surface_active {
            self.app.navigate_bookmark(workspace_id);
        }
        moved
    }

    fn focus_target_for_surface(&self, view: &ViewedWorkspace<'_>) -> ShellFocusTarget {
        ShellFocusTarget {
            workspace_id: view.workspace.id(),
            pane_id: view.workspace.tree().focused(),
        }
    }

    /// The client an automatically created workspace is sized for and
    /// controlled by: `trigger` if it presents a surface, else the presenting
    /// client with the lowest id. `None` when no client presents one, and the
    /// workspace is sized for the headless area with no controller.
    fn automatic_creation_source(&self, trigger: Option<ClientId>) -> Option<ClientId> {
        trigger
            .filter(|client_id| self.clients.is_presenting(client_id))
            .or_else(|| {
                self.clients
                    .presenting()
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
        if !self.app.state().workspaces().is_empty() {
            self.schedule.creation.reset();
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
        let now = self.app.clock().now;
        if !self.schedule.creation.may_attempt(now) {
            return false;
        }
        match self.app.create_default_workspace(geometry) {
            DefaultWorkspace::Created => self.schedule.creation.reset(),
            DefaultWorkspace::Exists => {
                self.schedule.creation.reset();
                return false;
            }
            DefaultWorkspace::Failed => {
                self.schedule.creation.failed(now);
                return false;
            }
        }
        self.immediate_pty_sources_dirty = true;
        if let (Some(client_id), Some(workspace)) =
            (source, self.app.state().workspaces().as_slice().last())
        {
            self.clients
                .set_geometry_controller(workspace.id(), client_id);
        }
        self.reconcile_client_shell_locations();
        self.reapply_controlled_shell_workspace_geometry(PendingResumes::Defer);
        self.sync_pane_focus();
        true
    }

    /// The pane a client's keyboard reaches: the focused pane of the workspace
    /// its location names.
    pub(super) fn shell_focus_target(&self, client_id: ClientId) -> Option<ShellFocusTarget> {
        Some(self.focus_target_for_surface(&self.viewed_workspace_for_client(client_id)?))
    }

    /// The panes that hold terminal focus: the focus target of every active
    /// shell client whose outer terminal reported focus. Several viewers of
    /// one pane focus it once.
    fn panes_holding_focus(&self) -> HashSet<ShellFocusTarget> {
        self.clients
            .presenting()
            .filter(|(_, client)| client.shell_state().outer_terminal_focus.is_focused())
            .filter_map(|(&client_id, _)| self.shell_focus_target(client_id))
            .collect()
    }

    /// Reports focus changes to the panes (`CSI I` / `CSI O` for panes that
    /// enabled focus reporting): every pane that stopped being focused loses
    /// focus and every newly focused pane gains it. Level-based, so it is
    /// correct whatever changed the views (navigation, a client leaving, a
    /// pane dying, an outer focus report) and idempotent; the handlers call
    /// it once their change is applied.
    pub(super) fn sync_pane_focus(&mut self) {
        self.sync_pane_focus_after(&[]);
    }

    /// [`Self::sync_pane_focus`] after `replaced` panes got a new runtime (an
    /// agent resume starting its shell).
    pub(super) fn sync_pane_focus_after(&mut self, replaced: &[shepr_core::layout::PaneId]) {
        let focused = self.panes_holding_focus();
        // A pane whose runtime was replaced stays in the set, so the set diff
        // never tells the new runtime; it gets the focus-in report here. A
        // replaced pane that newly gains focus is told by the diff below.
        for &pane_id in replaced {
            let Some(workspace_id) = self
                .app
                .state()
                .pane(pane_id)
                .map(|pane| pane.workspace().id())
            else {
                continue;
            };
            let key = ShellFocusTarget {
                workspace_id,
                pane_id,
            };
            if focused.contains(&key) && self.focused_panes.contains(&key) {
                self.app
                    .send_pane_focus_event(pane_id, shepr_vt::FocusEvent::Gained);
            }
        }
        if focused == self.focused_panes {
            return;
        }
        for target in self.focused_panes.difference(&focused) {
            self.app
                .send_pane_focus_event(target.pane_id, shepr_vt::FocusEvent::Lost);
        }
        for target in focused.difference(&self.focused_panes) {
            self.app
                .send_pane_focus_event(target.pane_id, shepr_vt::FocusEvent::Gained);
        }
        self.focused_panes = focused;
    }

    /// The attached active shell clients viewing `pane_id`, in id order.
    /// Clipboard writes, scroll invalidation and refused-surface retries use
    /// the same visibility rule.
    pub(super) fn pane_viewers(&self, pane_id: shepr_core::layout::PaneId) -> Vec<ClientId> {
        let Some(workspace_id) = self
            .app
            .state()
            .pane(pane_id)
            .map(|pane| pane.workspace().id())
        else {
            return Vec::new();
        };
        self.clients
            .presenting()
            .map(|(&client_id, _)| client_id)
            .filter(|&client_id| self.shell_client_views_pane(client_id, &workspace_id, pane_id))
            .collect()
    }

    pub(super) fn shell_client_views_pane(
        &self,
        client_id: ClientId,
        workspace_id: &shepr_protocol::WorkspaceId,
        pane_id: shepr_core::layout::PaneId,
    ) -> bool {
        let Some(target) = self.shell_target_for_client(client_id) else {
            return false;
        };
        if &target != workspace_id {
            return false;
        }
        self.app
            .state()
            .workspace(workspace_id)
            .is_some_and(|workspace| workspace.tree().shows(pane_id))
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
        Some(SpawnGeometry::for_grid(
            client.terminal_size,
            client.host_cell.cell(),
        ))
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
    /// and records the geometry on the session. Returns whether the recorded
    /// workspace geometry changed.
    pub(super) fn apply_workspace_geometry(
        &mut self,
        workspace_id: &shepr_protocol::WorkspaceId,
    ) -> bool {
        if self.app.state().workspace(workspace_id).is_none() {
            return false;
        }
        let Some(geometry) = self.workspace_geometry(workspace_id) else {
            return false;
        };
        self.app.apply_workspace_geometry(workspace_id, geometry)
    }

    /// The runtimes of the panes a surface of the workspace `target` names
    /// shows: the focused pane when zoomed, every layout pane otherwise.
    pub(super) fn visible_pane_runtimes(
        &self,
        target: &shepr_protocol::WorkspaceId,
    ) -> impl Iterator<Item = &shepr_mux::pane::PaneRuntime> + '_ {
        ViewedWorkspace::resolve(&self.app, target)
            .into_iter()
            .flat_map(move |view| self.runtimes_shown_by(view.workspace))
    }

    /// The runtimes of the panes `workspace` shows, for a caller that already
    /// holds the resolved workspace.
    pub(super) fn runtimes_shown_by<'a>(
        &'a self,
        workspace: &shepr_mux::workspace::Workspace,
    ) -> impl Iterator<Item = &'a shepr_mux::pane::PaneRuntime> + 'a {
        workspace
            .tree()
            .visible_pane_ids()
            .into_iter()
            .filter_map(move |pane_id| self.app.pane_runtime(pane_id))
    }

    /// Whether a visible pane of the workspace `target` names is inside a
    /// synchronized update, which a resize would tear. Reads the lock-free
    /// mirror, so it never waits on a PTY reader.
    pub(super) fn workspace_has_synchronized_pane(
        &self,
        target: &shepr_protocol::WorkspaceId,
    ) -> bool {
        self.visible_pane_runtimes(target)
            .any(|runtime| runtime.read().synchronized_output_active())
    }

    /// Every presenting client with the workspace it views, each resolved once,
    /// in client id order. A client viewing nothing is left out.
    pub(super) fn presented_views(
        &self,
    ) -> impl Iterator<Item = (ClientId, ViewedWorkspace<'_>)> + '_ {
        self.clients
            .presenting()
            .filter_map(|(&client_id, client)| {
                let view =
                    ViewedWorkspace::for_location(&self.app, &client.shell_state().location)?;
                Some((client_id, view))
            })
    }

    /// The visible panes' PTY grid sizes, to tell whether a geometry
    /// application resized any of them.
    pub(super) fn visible_pane_grid_sizes(
        &self,
        target: &shepr_protocol::WorkspaceId,
    ) -> Vec<shepr_core::geometry::GridSize> {
        self.visible_pane_runtimes(target)
            .map(shepr_mux::pane::PaneRuntime::grid_size)
            .collect()
    }

    /// Settle layout before deciding what any client is owed: lay out a
    /// workspace that has no recorded area yet, and re-apply the geometry of
    /// one where output flipped a visible pane's active screen (pane chrome
    /// differs between the screens, so the PTY size can change).
    ///
    /// The flip is reported by the pane's parse path as a lock-free flag, and
    /// any output that sets it also raises the render signal, so the flags are
    /// only read on a plan with that signal pending (`pty_dirty`). This runs
    /// at the top of every plan, so it must not poll terminal cores for their
    /// screen mode: that took two core locks per visible pane of every
    /// workspace on every output wake. Nor can delivered client baselines
    /// serve as the record: a slow or refused surface may retain an old mode
    /// indefinitely. A workspace skipped for a synchronized update keeps its
    /// flags, and the update's end raises the signal that retries it.
    pub(super) fn settle_workspace_geometry_before_plan(&mut self, pty_dirty: bool) {
        // By position, re-read each step: applying geometry neither adds nor
        // moves workspaces, and no list of them is built.
        let mut index = 0;
        while let Some(workspace) = self.app.state().workspaces().as_slice().get(index) {
            index += 1;
            let workspace_id = workspace.id();
            let missing_area = workspace.spawn_geometry().is_none();
            let flipped = pty_dirty
                && self
                    .runtimes_shown_by(workspace)
                    .any(|runtime| runtime.read().screen_flip_pending());
            if !missing_area && !flipped {
                continue;
            }
            if self.workspace_geometry_source(&workspace_id).is_none()
                || self.workspace_has_synchronized_pane(&workspace_id)
            {
                continue;
            }
            // Taken before applying, so a flip that lands meanwhile is kept
            // for the next plan.
            for runtime in self.visible_pane_runtimes(&workspace_id) {
                runtime.take_screen_flip();
            }
            let sizes_before = self.visible_pane_grid_sizes(&workspace_id);
            let area_changed = self.apply_workspace_geometry(&workspace_id);
            if !area_changed && self.visible_pane_grid_sizes(&workspace_id) == sizes_before {
                continue;
            }
            // Only viewers of panes whose geometry changed need to recompute.
            // This runs before the plan, so every affected client is included
            // regardless of another viewer's delivery slot or scroll baseline.
            self.request_recompute_of_viewers(&workspace_id);
        }
    }

    /// Has every client whose location names the live workspace `workspace_id`
    /// recompute its surface. The workspace is known to exist, so comparing the
    /// location's id is the whole resolution.
    fn request_recompute_of_viewers(&mut self, workspace_id: &shepr_protocol::WorkspaceId) {
        for client in self.clients.values_mut() {
            if client.shell_state().location.focused_workspace_id() == Some(workspace_id) {
                client.request_recompute();
            }
        }
    }

    /// Applies the PTY size rule to every workspace. Pane runtimes ignore an
    /// unchanged pane size; the result reports whether any pane size or
    /// recorded workspace geometry changed.
    pub(super) fn apply_all_workspace_geometry(&mut self) -> bool {
        let mut changed = false;
        // By position, re-read each step: applying geometry neither adds nor
        // moves workspaces.
        let mut index = 0;
        while let Some(workspace_id) = self
            .app
            .state()
            .workspaces()
            .as_slice()
            .get(index)
            .map(shepr_mux::workspace::Workspace::id)
        {
            index += 1;
            let sizes_before = self.visible_pane_grid_sizes(&workspace_id);
            let area_changed = self.apply_workspace_geometry(&workspace_id);
            let resized =
                area_changed || self.visible_pane_grid_sizes(&workspace_id) != sizes_before;
            if resized {
                self.request_recompute_of_viewers(&workspace_id);
            }
            changed |= resized;
        }
        changed
    }

    fn finish_shell_workspace_geometry_change(
        &mut self,
        geometry_changed: bool,
        pending_resumes: PendingResumes,
    ) -> bool {
        if pending_resumes == PendingResumes::Defer {
            return geometry_changed;
        }
        let now = self.app.clock().now;
        let resumed = self.app.start_pending_agent_resumes(now);
        if resumed.consumed {
            for client in self.clients.values_mut() {
                client.request_recompute();
            }
            self.sync_pane_focus_after(&resumed.replaced_runtimes);
        }
        geometry_changed || resumed.consumed
    }

    /// Applies the PTY size rule to every workspace and has its viewers
    /// recompute when pane or recorded geometry changed. Pending resumes are
    /// settled even when this application repeats the current geometry.
    fn apply_shell_geometry(&mut self, pending_resumes: PendingResumes) -> bool {
        let geometry_changed = self.apply_all_workspace_geometry();
        self.finish_shell_workspace_geometry_change(geometry_changed, pending_resumes)
    }

    /// Whether the PTY size rule sizes some workspace for `client_id`.
    fn is_geometry_source(&self, client_id: ClientId) -> bool {
        self.app.state().workspaces().iter().any(|workspace| {
            self.workspace_geometry_source(&workspace.id())
                == Some(GeometrySource::Client(client_id))
        })
    }

    /// Settles a stale controller to the lowest-id outer-focused viewer, or
    /// the lowest-id viewer when none is focused, then applies the view-derived
    /// PTY size rule to every workspace.
    pub(super) fn reapply_controlled_shell_workspace_geometry(
        &mut self,
        pending_resumes: PendingResumes,
    ) -> bool {
        // Persist exactly the source selected by the PTY size rule. Remember
        // only viewers: a sole presenter sizes hidden workspaces without
        // taking their next viewer's claim away.
        for workspace_id in self.workspace_order() {
            if let Some(GeometrySource::Client(client_id)) =
                self.workspace_geometry_source(&workspace_id)
                && self.shell_target_for_client(client_id).as_ref() == Some(&workspace_id)
            {
                self.clients
                    .set_geometry_controller(workspace_id, client_id);
            }
        }
        self.apply_shell_geometry(pending_resumes)
    }

    /// Owns claim policy as well as the geometry settlement required by each reason.
    pub(super) fn claim_client_geometry(
        &mut self,
        client_id: ClientId,
        reason: GeometryClaimReason,
    ) -> bool {
        match reason {
            GeometryClaimReason::Connect => {
                self.claim_unowned_shell_workspace_geometry(client_id, PendingResumes::Start)
            }
            GeometryClaimReason::Activate => {
                let resized =
                    self.resize_shell_workspaces_sized_for(client_id, PendingResumes::Start);
                let focused_viewer =
                    self.shell_target_for_client(client_id)
                        .is_some_and(|workspace| {
                            self.clients.presenting().any(|(&other_id, client)| {
                                other_id != client_id
                                    && client.shell_state().outer_terminal_focus.is_focused()
                                    && self.shell_target_for_client(other_id) == Some(workspace)
                            })
                        });
                let claimed = !focused_viewer
                    && self.claim_shell_workspace_geometry(client_id, PendingResumes::Start);
                resized || claimed
            }
            GeometryClaimReason::Focus | GeometryClaimReason::Interaction => {
                self.claim_shell_workspace_geometry(client_id, PendingResumes::Defer)
            }
            GeometryClaimReason::Command {
                claims,
                topology,
                navigated,
            } => {
                if !claims {
                    return false;
                }
                if topology {
                    self.reapply_controlled_shell_workspace_geometry(PendingResumes::Defer)
                } else if navigated {
                    if let Some(workspace) = self.shell_target_for_client(client_id) {
                        self.clients.claim_geometry(workspace, client_id);
                    }
                    // Navigation must settle even when the destination remembers this owner.
                    self.reapply_controlled_shell_workspace_geometry(PendingResumes::Defer)
                } else {
                    self.claim_shell_workspace_geometry(client_id, PendingResumes::Defer)
                        || self.resize_shell_workspaces_sized_for(client_id, PendingResumes::Defer)
                }
            }
        }
    }

    /// Makes `client_id` the geometry controller of the workspace it views
    /// and, if that changed the controller, applies the PTY size rule.
    pub(super) fn claim_shell_workspace_geometry(
        &mut self,
        client_id: ClientId,
        pending_resumes: PendingResumes,
    ) -> bool {
        let Some(workspace_id) = self.shell_target_for_client(client_id) else {
            return false;
        };
        if !self.clients.claim_geometry(workspace_id, client_id) {
            return false;
        }
        self.apply_shell_geometry(pending_resumes)
    }

    /// As `claim_shell_workspace_geometry`, for a workspace no client controls
    /// yet.
    pub(super) fn claim_unowned_shell_workspace_geometry(
        &mut self,
        client_id: ClientId,
        pending_resumes: PendingResumes,
    ) -> bool {
        let Some(workspace_id) = self.shell_target_for_client(client_id) else {
            return false;
        };
        if !self.clients.claim_unowned_geometry(workspace_id, client_id) {
            return false;
        }
        self.apply_shell_geometry(pending_resumes)
    }

    /// Re-applies the PTY size rule after `client_id`'s own geometry changed
    /// (a resize, a new cell size), when some workspace is sized for it.
    pub(super) fn resize_shell_workspaces_sized_for(
        &mut self,
        client_id: ClientId,
        pending_resumes: PendingResumes,
    ) -> bool {
        if !self.is_geometry_source(client_id) {
            return false;
        }
        self.apply_shell_geometry(pending_resumes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(active: bool) -> ClientConnection {
        let outbox = crate::server::outbox::ClientOutbox::test_pair().0;
        ClientConnection::with_shell(
            ClientShellState::with_surface_active(active),
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_core::geometry::HostCell::Unknown,
            crate::server::clients::ActivityStamp::from(1),
            outbox,
        )
    }

    #[test]
    fn pty_size_rule_prefers_focused_viewer_when_controller_is_stale() {
        let workspace_id: shepr_protocol::WorkspaceId = shepr_test_fixtures::id("w1");
        let mut clients = crate::server::clients::ClientRegistry::default();
        assert_eq!(
            workspace_geometry_source(&clients, &workspace_id),
            Some(GeometrySource::Headless)
        );

        let first = ClientId::test_new(1);
        let mut first_client = client(true);
        first_client
            .shell_state_mut()
            .location
            .navigate(workspace_id, 0);
        clients.insert(first, first_client);
        assert_eq!(
            workspace_geometry_source(&clients, &workspace_id),
            Some(GeometrySource::Client(first))
        );

        // With a second presenter, the current viewers decide the source.
        let second = ClientId::test_new(2);
        let mut second_client = client(true);
        second_client
            .shell_state_mut()
            .location
            .navigate(workspace_id, 0);
        second_client.shell_state_mut().outer_terminal_focus =
            crate::server::clients::OuterFocus::Focused;
        clients.insert(second, second_client);
        assert_eq!(
            workspace_geometry_source(&clients, &workspace_id),
            Some(GeometrySource::Client(second)),
            "an outer-focused viewer wins the fallback over a lower-id viewer"
        );
        assert!(clients.claim_geometry(workspace_id, first));
        assert_eq!(
            workspace_geometry_source(&clients, &workspace_id),
            Some(GeometrySource::Client(first)),
            "a remembered controller wins while it still views the workspace"
        );

        let other_workspace_id: shepr_protocol::WorkspaceId = shepr_test_fixtures::id("w2");
        if let Some(client) = clients.get_mut(&first) {
            client
                .shell_state_mut()
                .location
                .navigate(other_workspace_id, 0);
        }
        assert_eq!(
            workspace_geometry_source(&clients, &workspace_id),
            Some(GeometrySource::Client(second)),
            "a focused viewer wins after the remembered controller leaves"
        );

        // An inactive surface presents nothing: the other one is sole again.
        assert!(matches!(
            clients.set_surface_active(second, false),
            Some(crate::server::clients::ClientSurfaceChange {
                changed: true,
                departure: Some(_),
                ..
            })
        ));
        assert_eq!(
            workspace_geometry_source(&clients, &workspace_id),
            Some(GeometrySource::Client(first))
        );

        // A metadata-only connection never sizes anything.
        assert!(clients.remove_client(first).is_some());
        assert_eq!(
            workspace_geometry_source(&clients, &workspace_id),
            Some(GeometrySource::Headless)
        );
    }
}
