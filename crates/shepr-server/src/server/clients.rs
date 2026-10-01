use std::collections::HashMap;
use std::ops::Index;

use crate::server::outbox::ClientOutbox;
use crate::server::render_stream::ClientRenderState;
use shepr_protocol::WorkspaceId;
use shepr_protocol::{
    ClientKeyCode, ClientKeyKind, ClientMouseButton, ClientMouseKind, ClientPaneInputEvent,
};

/// Typed identity paired with one pane in a client's committed surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientPaneIdentity {
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) pane_id: shepr_core::layout::PaneId,
}

/// Identity of a connection accepted by this server. Only the shared
/// allocator mints production values; disconnecting never reuses one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(u64);

/// Shared allocator for transport threads; identities are never reused.
#[derive(Clone)]
pub(crate) struct ClientIdAllocator(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl Default for ClientIdAllocator {
    fn default() -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)))
    }
}

impl ClientIdAllocator {
    pub(crate) fn allocate(&self) -> ClientId {
        ClientId(self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

/// Monotonic ordering of accepted client activity within one server run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ActivityStamp(u64);

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Default)]
pub(crate) struct ClientShellState {
    /// Whether this shell currently receives pane surfaces.
    pub(crate) surface_active: bool,
    /// Whether this shell wants host mouse capture without pane demand.
    pub(crate) mouse_capture: bool,
    /// Last host terminal default colors reported by this shell.
    pub(crate) host_terminal_theme: shepr_termio::host_term::theme::TerminalTheme,
    /// Last host light/dark appearance reported by this shell.
    pub(crate) host_terminal_appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
    /// Whether appearance came from an explicit host color-scheme report.
    pub(crate) host_terminal_appearance_explicit: bool,
    /// Last reported focus state for this shell's outer terminal.
    pub(crate) outer_terminal_focus: Option<bool>,
    /// Presses forwarded by this shell that need release on abrupt teardown,
    /// keyed by target pane and the client's reported press identity. The
    /// client pins a pane mouse gesture's drag and release to its original
    /// target even when the pointer crosses a pane boundary.
    held_inputs: HashMap<(shepr_protocol::PublicPaneId, ClientShellPressId), ClientShellHeldInput>,
    /// The workspace this connection views. Only this client's own navigation,
    /// and the settling of workspaces that appeared or vanished, move it.
    pub(crate) location: ClientShellLocation,
    /// Last coherent shell replacement sent to this client.
    pub(crate) snapshot: Option<shepr_protocol::ClientShellSnapshot>,
    /// Shared session-cache generation projected for this connection.
    pub(crate) session_generation: u64,
    /// The generation of `location` the last successful projection carried. A
    /// location change moves only this client's generation, so only its
    /// projection is invalidated.
    pub(crate) projected_location_generation: u64,
    /// Monotonic shell replacement revision for this connection.
    pub(crate) projection_revision: shepr_protocol::ProjectionRevision,
}

impl ClientShellState {
    pub(crate) fn active() -> Self {
        Self {
            surface_active: true,
            ..Self::default()
        }
    }

    fn update_host_theme(&mut self, update: &shepr_protocol::ClientHostThemeUpdate) -> bool {
        let mut next_theme = self.host_terminal_theme;
        let mut changed = false;

        match update {
            shepr_protocol::ClientHostThemeUpdate::DefaultColor { kind, color } => {
                let kind: shepr_termio::host_term::theme::DefaultColorKind = (*kind).into();
                let color = (*color).into();
                next_theme = next_theme.with_color(kind, color);
                if matches!(
                    kind,
                    shepr_termio::host_term::theme::DefaultColorKind::Background
                ) && !self.host_terminal_appearance_explicit
                {
                    changed |= self.set_host_appearance(Some(color.inferred_appearance()), false);
                }
            }
            shepr_protocol::ClientHostThemeUpdate::PaletteColors(colors) => {
                for &(index, color) in colors {
                    next_theme = next_theme.with_palette_color(index, color.into());
                }
            }
            shepr_protocol::ClientHostThemeUpdate::Appearance(appearance) => {
                let appearance = (*appearance).into();
                changed |= self.set_host_appearance(Some(appearance), true);
            }
        }

        if next_theme != self.host_terminal_theme {
            self.host_terminal_theme = next_theme;
            changed = true;
        }
        changed
    }

    fn set_host_appearance(
        &mut self,
        appearance: Option<shepr_termio::host_term::theme::HostAppearance>,
        explicit: bool,
    ) -> bool {
        if self.host_terminal_appearance_explicit && !explicit {
            return false;
        }
        if self.host_terminal_appearance == appearance
            && self.host_terminal_appearance_explicit == explicit
        {
            return false;
        }
        self.host_terminal_appearance = appearance;
        self.host_terminal_appearance_explicit = explicit;
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderTarget {
    pub(crate) client_id: ClientId,
    pub(crate) terminal_size: shepr_core::geometry::GridSize,
    pub(crate) cell_size: shepr_termio::host_term::cell_size::HostCellSize,
}

/// Pure client identity and ownership state for one headless server.
///
/// Connection accessors support the transport and rendering paths that need to
/// inspect a connection, while cross-connection decisions and ownership maps
/// live here and can be tested without a PTY.
///
/// Presentation (surface size, outer focus, location, window title, input
/// modes) lives on each connection, and so does what the connection is owed
/// (its render state's settle point and surface debt); nothing here or in the
/// app mirrors one client's view as a session-wide one. The registry holds two arbitrations
/// between clients: which one controls each workspace's PTY geometry, and which
/// active shell most recently recorded user activity (the foreground client).
/// Connection or surface activation, outer focus gain, pane interaction, and
/// endpoint commands record activity; a surface resize only changes geometry.
/// The foreground client supplies the host theme panes are coloured with, the
/// one effect a pane has one of whichever client views it, and receives
/// clipboard writes from panes that no client views.
pub(crate) struct ClientRegistry {
    connections: HashMap<ClientId, ClientConnection>,
    foreground_client_id: Option<ClientId>,
    geometry_controllers: HashMap<WorkspaceId, ClientId>,
    next_activity_stamp: u64,
}

impl Default for ClientRegistry {
    fn default() -> Self {
        Self {
            connections: HashMap::new(),
            foreground_client_id: None,
            geometry_controllers: HashMap::new(),
            next_activity_stamp: 1,
        }
    }
}

impl<'a> IntoIterator for &'a ClientRegistry {
    type Item = (&'a ClientId, &'a ClientConnection);
    type IntoIter = std::collections::hash_map::Iter<'a, ClientId, ClientConnection>;

    fn into_iter(self) -> Self::IntoIter {
        self.connections.iter()
    }
}

impl<'a> IntoIterator for &'a mut ClientRegistry {
    type Item = (&'a ClientId, &'a mut ClientConnection);
    type IntoIter = std::collections::hash_map::IterMut<'a, ClientId, ClientConnection>;

    fn into_iter(self) -> Self::IntoIter {
        self.connections.iter_mut()
    }
}

impl<K: Copy + Into<ClientId>> Index<&K> for ClientRegistry {
    type Output = ClientConnection;

    fn index(&self, client_id: &K) -> &Self::Output {
        &self.connections[&(*client_id).into()]
    }
}

impl ClientRegistry {
    pub(crate) fn contains_key<K: Copy + Into<ClientId>>(&self, client_id: &K) -> bool {
        self.connections.contains_key(&(*client_id).into())
    }

    pub(crate) fn get<K: Copy + Into<ClientId>>(&self, client_id: &K) -> Option<&ClientConnection> {
        self.connections.get(&(*client_id).into())
    }

    pub(crate) fn get_mut<K: Copy + Into<ClientId>>(
        &mut self,
        client_id: &K,
    ) -> Option<&mut ClientConnection> {
        self.connections.get_mut(&(*client_id).into())
    }

    pub(crate) fn insert(
        &mut self,
        client_id: impl Into<ClientId>,
        client: ClientConnection,
    ) -> Option<ClientConnection> {
        self.connections.insert(client_id.into(), client)
    }

    pub(crate) fn keys(&self) -> std::collections::hash_map::Keys<'_, ClientId, ClientConnection> {
        self.connections.keys()
    }

    pub(crate) fn values(
        &self,
    ) -> std::collections::hash_map::Values<'_, ClientId, ClientConnection> {
        self.connections.values()
    }

    pub(crate) fn values_mut(
        &mut self,
    ) -> std::collections::hash_map::ValuesMut<'_, ClientId, ClientConnection> {
        self.connections.values_mut()
    }

    pub(crate) fn iter(&self) -> std::collections::hash_map::Iter<'_, ClientId, ClientConnection> {
        self.connections.iter()
    }

    pub(crate) fn allocate_activity_stamp(&mut self) -> ActivityStamp {
        let stamp = self.next_activity_stamp;
        self.next_activity_stamp = self.next_activity_stamp.saturating_add(1);
        ActivityStamp(stamp)
    }

    pub(crate) fn foreground_client_id(&self) -> Option<ClientId> {
        self.foreground_client_id
    }

    pub(crate) fn latest_shell_client(&self) -> Option<ClientId> {
        latest_shell_client(&self.connections)
    }

    pub(crate) fn promote_to_foreground(&mut self, client_id: ClientId) -> bool {
        let stamp = self.allocate_activity_stamp();
        let Some(client) = self.connections.get_mut(&client_id) else {
            return false;
        };
        if !client.is_active_shell_client() {
            return false;
        }
        client.last_activity = stamp;
        let changed = self.foreground_client_id != Some(client_id);
        self.foreground_client_id = Some(client_id);
        changed
    }

    pub(crate) fn promote_latest_remaining(&mut self) -> bool {
        let next = latest_shell_client(&self.connections);
        let changed = next != self.foreground_client_id;
        self.foreground_client_id = next;
        changed
    }

    pub(crate) fn app_client_count(&self) -> usize {
        self.connections
            .values()
            .filter(|client| client.is_active_shell_client() && client.outbox.is_attached())
            .count()
    }

    pub(crate) fn remove_client(
        &mut self,
        client_id: ClientId,
    ) -> (Option<ClientConnection>, bool) {
        let was_foreground = self.foreground_client_id == Some(client_id);
        let removed = self.connections.remove(&client_id);
        if let Some(removed) = &removed {
            // The reader thread holds a control sender on the same queue, so
            // dropping the outbox cannot by itself end the transport lifetime.
            removed.outbox.close();
        }
        self.remove_geometry_controllers_for(client_id);
        if was_foreground {
            self.foreground_client_id = None;
        }
        (removed, was_foreground)
    }

    pub(crate) fn clear(&mut self) {
        // Shutdown has queued its notice and flush barrier by now. Dropping
        // an outbox does not close it: its writer drains what is queued
        // before it notices that no sender is left.
        self.connections.clear();
        self.foreground_client_id = None;
        self.geometry_controllers.clear();
    }

    pub(crate) fn geometry_controller(&self, workspace_id: &WorkspaceId) -> Option<ClientId> {
        self.geometry_controllers.get(workspace_id).copied()
    }

    pub(crate) fn set_geometry_controller(
        &mut self,
        workspace_id: WorkspaceId,
        client_id: ClientId,
    ) -> Option<ClientId> {
        self.geometry_controllers.insert(workspace_id, client_id)
    }

    pub(crate) fn claim_geometry(
        &mut self,
        workspace_id: WorkspaceId,
        client_id: ClientId,
    ) -> bool {
        if !self
            .connections
            .get(&client_id)
            .is_some_and(ClientConnection::is_active_shell_client)
        {
            return false;
        }
        self.geometry_controllers.insert(workspace_id, client_id) != Some(client_id)
    }

    pub(crate) fn claim_unowned_geometry(
        &mut self,
        workspace_id: WorkspaceId,
        client_id: ClientId,
    ) -> bool {
        if !self
            .connections
            .get(&client_id)
            .is_some_and(ClientConnection::is_active_shell_client)
        {
            return false;
        }
        if self.geometry_controllers.contains_key(&workspace_id) {
            return false;
        }
        self.geometry_controllers.insert(workspace_id, client_id);
        true
    }

    pub(crate) fn retain_geometry_controllers(
        &mut self,
        mut keep: impl FnMut(&WorkspaceId, ClientId) -> bool,
    ) {
        self.geometry_controllers
            .retain(|workspace_id, client_id| keep(workspace_id, *client_id));
    }

    pub(crate) fn remove_geometry_controllers_for(&mut self, client_id: ClientId) {
        self.geometry_controllers
            .retain(|_, controller_id| *controller_id != client_id);
    }
}

/// A held press identity within one target pane, keyed by what the client
/// reports: the key code (a Linux terminal reports no physical key identity)
/// or the mouse button.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ClientShellPressId {
    Key(ClientKeyCode),
    Mouse(ClientMouseButton),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClientShellHeldInput {
    pub(crate) target: shepr_protocol::PublicPaneId,
    pub(crate) release: ClientPaneInputEvent,
}

/// Which workspace one client views, with the index that workspace last had
/// and a generation that moves whenever the viewed workspace changes.
///
/// The remembered index is what a client whose workspace vanishes lands by:
/// the workspace now at that index, clamped. It is refreshed on every order
/// change and navigation without moving the generation, since the client still
/// views the same workspace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClientShellLocation {
    pub(crate) focused_workspace_id: Option<WorkspaceId>,
    index: usize,
    generation: u64,
}

/// The session's workspaces in order, and where a client with no location
/// starts, for settling every client's location after they changed.
pub(crate) struct ClientShellTopology {
    pub(crate) workspace_ids: Vec<WorkspaceId>,
    /// The index of the bookmarked workspace, if it is a workspace.
    pub(crate) bookmark_index: Option<usize>,
}

impl ClientShellLocation {
    /// A new client's starting location: the workspace at `start` (the
    /// session's bookmark), if any. Initialising counts as a change.
    pub(crate) fn initial(start: Option<(WorkspaceId, usize)>) -> Self {
        let mut location = Self::default();
        location.set(start);
        location
    }

    /// The generation of the viewed workspace: it moves on every change of
    /// which workspace this is, and only then.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Moves to `workspace_id`, now at `index`. Returns whether the viewed
    /// workspace changed (the generation moved); navigating to the workspace
    /// already viewed only refreshes its index.
    pub(crate) fn navigate(&mut self, workspace_id: WorkspaceId, index: usize) -> bool {
        self.set(Some((workspace_id, index)))
    }

    fn set(&mut self, workspace: Option<(WorkspaceId, usize)>) -> bool {
        let (id, index) = match workspace {
            Some((id, index)) => (Some(id), index),
            None => (None, 0),
        };
        let changed = self.focused_workspace_id != id;
        self.focused_workspace_id = id;
        self.index = index;
        if changed {
            self.generation = self.generation.saturating_add(1);
        }
        changed
    }

    /// Settles this location against the workspaces after they changed.
    /// Returns whether the viewed workspace changed.
    ///
    /// A workspace that is still there stays viewed, and only its index is
    /// refreshed. One that vanished is replaced by the workspace now at its
    /// remembered index, clamped to the last one, or by nothing when none is
    /// left. A client viewing nothing while workspaces exist starts on the
    /// bookmark, else on the first workspace.
    pub(crate) fn reconcile(&mut self, topology: &ClientShellTopology) -> bool {
        let landed = match &self.focused_workspace_id {
            Some(id) => match topology
                .workspace_ids
                .iter()
                .position(|candidate| candidate == id)
            {
                Some(index) => Some(index),
                None => topology
                    .workspace_ids
                    .len()
                    .checked_sub(1)
                    .map(|last| self.index.min(last)),
            },
            None => topology
                .bookmark_index
                .or_else(|| (!topology.workspace_ids.is_empty()).then_some(0)),
        };
        self.set(landed.map(|index| (topology.workspace_ids[index].clone(), index)))
    }
}

/// A connected client tracked by the server.
pub(crate) struct ClientConnection {
    /// Shell state of this connection; every client is a shell.
    pub(crate) shell: ClientShellState,
    /// The client's terminal size after clamping.
    pub(crate) terminal_size: shepr_core::geometry::GridSize,
    /// Pixel size of one client terminal cell.
    pub(crate) cell_size: shepr_termio::host_term::cell_size::HostCellSize,
    /// Monotonic activity stamp used to choose the fallback foreground client.
    pub(crate) last_activity: ActivityStamp,
    /// Render baseline for the negotiated client encoding.
    pub(crate) render_state: ClientRenderState,
    /// Typed identities aligned with the panes in `render_state`'s baseline.
    pub(crate) surface_pane_identities: Vec<ClientPaneIdentity>,
    /// Whether this frontend preserves exact SGR pixel reports.
    pub(crate) pixel_mouse: bool,
    /// Whether the client has been told that its current surface is too large
    /// to send even in parts (past `MAX_MESSAGE_SIZE`). Set on the first
    /// oversized surface, cleared once a surface goes out, so a client whose
    /// surfaces keep failing is warned once rather than per render.
    pub(crate) oversized_surface_reported: bool,
    /// Outgoing messages, held replies and presentation state for this connection.
    pub(crate) outbox: ClientOutbox,
}

impl ClientConnection {
    pub(crate) fn with_shell(
        shell: ClientShellState,
        terminal_size: shepr_core::geometry::GridSize,
        cell_size: shepr_termio::host_term::cell_size::HostCellSize,
        last_activity: impl Into<ActivityStamp>,
        outbox: ClientOutbox,
    ) -> Self {
        Self {
            shell,
            terminal_size,
            cell_size,
            last_activity: last_activity.into(),
            render_state: ClientRenderState::new(),
            surface_pane_identities: Vec::new(),
            pixel_mouse: false,
            oversized_surface_reported: false,
            outbox,
        }
    }

    pub(crate) fn shell_state(&self) -> &ClientShellState {
        &self.shell
    }

    pub(crate) fn shell_state_mut(&mut self) -> &mut ClientShellState {
        &mut self.shell
    }

    pub(crate) fn request_repaint(&mut self) {
        self.render_state.request_repaint();
        self.surface_pane_identities.clear();
    }

    pub(crate) fn commit_surface_pane_identities(&mut self, identities: Vec<ClientPaneIdentity>) {
        self.surface_pane_identities = identities;
    }

    pub(crate) fn request_recompute(&mut self) {
        self.render_state.request_recompute();
    }

    pub(crate) fn track_shell_input(
        &mut self,
        target: &shepr_protocol::PublicPaneId,
        events: &[ClientPaneInputEvent],
    ) {
        let shell = self.shell_state_mut();
        for event in events {
            match event {
                // A press that committed text gets no release from the client,
                // so only presses without generated text are held.
                ClientPaneInputEvent::Key {
                    code,
                    modifiers,
                    kind: ClientKeyKind::Press,
                    shifted_codepoint,
                    generated_text: None,
                    ..
                } => {
                    shell.held_inputs.insert(
                        (target.clone(), ClientShellPressId::Key(code.clone())),
                        ClientShellHeldInput {
                            target: target.to_owned(),
                            release: ClientPaneInputEvent::Key {
                                code: code.clone(),
                                modifiers: *modifiers,
                                kind: ClientKeyKind::Release,
                                repeat_count: 1,
                                shifted_codepoint: *shifted_codepoint,
                                generated_text: None,
                            },
                        },
                    );
                }
                ClientPaneInputEvent::Key {
                    code,
                    kind: ClientKeyKind::Release,
                    ..
                } => {
                    shell
                        .held_inputs
                        .remove(&(target.clone(), ClientShellPressId::Key(code.clone())));
                }
                ClientPaneInputEvent::Mouse {
                    kind: ClientMouseKind::Down(button),
                    position,
                    geometry,
                    modifiers,
                    ..
                }
                | ClientPaneInputEvent::Mouse {
                    kind: ClientMouseKind::Drag(button),
                    position,
                    geometry,
                    modifiers,
                    ..
                } => {
                    let id = (target.clone(), ClientShellPressId::Mouse(*button));
                    if matches!(
                        event,
                        ClientPaneInputEvent::Mouse {
                            kind: ClientMouseKind::Down(_),
                            ..
                        }
                    ) || shell.held_inputs.contains_key(&id)
                    {
                        shell.held_inputs.insert(
                            id,
                            ClientShellHeldInput {
                                target: target.to_owned(),
                                release: ClientPaneInputEvent::Mouse {
                                    kind: ClientMouseKind::Up(*button),
                                    position: *position,
                                    geometry: *geometry,
                                    modifiers: *modifiers,
                                    lines: 1,
                                },
                            },
                        );
                    }
                }
                ClientPaneInputEvent::Mouse {
                    kind: ClientMouseKind::Up(button),
                    ..
                } => {
                    shell
                        .held_inputs
                        .remove(&(target.clone(), ClientShellPressId::Mouse(*button)));
                }
                ClientPaneInputEvent::Key {
                    kind: ClientKeyKind::Press | ClientKeyKind::Repeat,
                    ..
                }
                | ClientPaneInputEvent::TextCommit(_)
                | ClientPaneInputEvent::Mouse { .. }
                | ClientPaneInputEvent::Paste(_) => {}
            }
        }
    }

    pub(crate) fn drain_shell_held_inputs(&mut self) -> Vec<ClientShellHeldInput> {
        let shell = self.shell_state_mut();
        shell.held_inputs.drain().map(|(_, held)| held).collect()
    }

    pub(crate) fn update_host_theme(
        &mut self,
        update: &shepr_protocol::ClientHostThemeUpdate,
    ) -> bool {
        self.shell_state_mut().update_host_theme(update)
    }

    pub(crate) fn is_active_shell_client(&self) -> bool {
        self.shell_state().surface_active
    }
}

pub(crate) fn latest_shell_client(
    clients: &HashMap<ClientId, ClientConnection>,
) -> Option<ClientId> {
    clients
        .iter()
        .filter(|(_, client)| client.is_active_shell_client())
        .max_by_key(|(_, client)| client.last_activity)
        .map(|(&client_id, _)| client_id)
}

/// Every connection with a writer, each rendered at its own surface size, in
/// a stable order.
pub(crate) fn render_targets(clients: &ClientRegistry) -> Vec<RenderTarget> {
    let mut targets: Vec<RenderTarget> = clients
        .iter()
        .filter(|(_, client)| client.outbox.is_attached())
        .map(|(&client_id, client)| RenderTarget {
            client_id,
            terminal_size: client.terminal_size,
            cell_size: client.cell_size,
        })
        .collect();

    targets.sort_by_key(|target| target.client_id);
    targets
}

#[cfg(test)]
impl From<u64> for ActivityStamp {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

#[cfg(test)]
impl From<i32> for ActivityStamp {
    fn from(value: i32) -> Self {
        Self(u64::try_from(value).expect("test activity stamp must be nonnegative"))
    }
}

#[cfg(test)]
impl From<u64> for ClientId {
    fn from(value: u64) -> Self {
        Self::test_new(value)
    }
}

#[cfg(test)]
impl From<i32> for ClientId {
    fn from(value: i32) -> Self {
        Self::test_new(u64::try_from(value).expect("test client id must be nonnegative"))
    }
}

#[cfg(test)]
impl PartialEq<u64> for ClientId {
    fn eq(&self, other: &u64) -> bool {
        self.0 == *other
    }
}

#[cfg(test)]
impl PartialEq<i32> for ClientId {
    fn eq(&self, other: &i32) -> bool {
        u64::try_from(*other).is_ok_and(|other| self.0 == other)
    }
}

#[cfg(test)]
impl ClientId {
    pub fn test_new(value: u64) -> Self {
        Self(value)
    }
}

#[cfg(test)]
impl ClientRegistry {
    /// Sets the foreground client without recording activity, as a fixture
    /// that has not been through the activity paths needs.
    pub(crate) fn set_foreground_client_id(&mut self, client_id: Option<ClientId>) {
        self.foreground_client_id = client_id;
    }
}

#[cfg(test)]
impl ClientShellLocation {
    /// The index the viewed workspace had when last seen.
    pub(crate) fn index(&self) -> usize {
        self.index
    }
}

#[cfg(test)]
impl ClientConnection {
    pub(crate) fn new(
        terminal_size: (u16, u16),
        cell_size: shepr_termio::host_term::cell_size::HostCellSize,
        last_activity: impl Into<ActivityStamp>,
        outbox: ClientOutbox,
    ) -> Self {
        Self::with_shell(
            ClientShellState::active(),
            shepr_core::geometry::GridSize::clamped(terminal_size.0, terminal_size.1),
            cell_size,
            last_activity,
            outbox,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell_client() -> ClientConnection {
        ClientConnection::new(
            (80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            1,
            crate::server::outbox::ClientOutbox::detached(),
        )
    }

    #[test]
    fn registry_owns_foreground_and_geometry_arbitration() {
        let mut registry = ClientRegistry::default();
        let ids = ClientIdAllocator::default();
        let first_id = ids.allocate();
        let second_id = ids.allocate();
        assert_eq!(
            (first_id, second_id),
            (ClientId::test_new(1), ClientId::test_new(2))
        );
        let first = shell_client();
        // A shell whose surface is not active never becomes the foreground.
        let second = ClientConnection::with_shell(
            ClientShellState::default(),
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_termio::host_term::cell_size::HostCellSize::default(),
            registry.allocate_activity_stamp(),
            crate::server::outbox::ClientOutbox::detached(),
        );
        registry.insert(first_id, first);
        registry.insert(second_id, second);

        assert!(registry.promote_to_foreground(first_id));
        assert_eq!(registry.foreground_client_id(), Some(first_id));
        assert!(!registry.promote_to_foreground(second_id));
        let workspace_id: WorkspaceId = shepr_test_fixtures::id("w1");
        assert!(registry.claim_geometry(workspace_id.clone(), first_id));
        assert!(!registry.claim_unowned_geometry(workspace_id.clone(), first_id));
        assert_eq!(registry.geometry_controller(&workspace_id), Some(first_id));

        let (_, was_foreground) = registry.remove_client(first_id);
        assert!(was_foreground);
        assert_eq!(registry.geometry_controller(&workspace_id), None);
        assert!(!registry.promote_latest_remaining());
        assert_eq!(registry.foreground_client_id(), None);
        let (removed, was_foreground) = registry.remove_client(second_id);
        assert!(removed.is_some());
        assert!(!was_foreground);
    }

    #[test]
    fn removing_a_client_closes_transport_handles_still_cloned_by_its_reader() {
        let mut registry = ClientRegistry::default();
        let ids = ClientIdAllocator::default();
        let client_id = ids.allocate();
        let (writer, control_rx, render_rx) = ClientOutbox::test_pair();
        let reader_control = writer.control_sender();
        registry.insert(
            client_id,
            ClientConnection::new(
                (80, 24),
                shepr_termio::host_term::cell_size::HostCellSize::default(),
                1,
                writer,
            ),
        );

        let (removed, _) = registry.remove_client(client_id);
        assert!(removed.is_some());
        assert_eq!(
            reader_control.send(&shepr_protocol::ServerMessage::HealthPong),
            crate::server::outbox::Delivery::Closed
        );
        drop(reader_control);
        assert!(matches!(
            control_rx.recv_timeout(std::time::Duration::from_secs(1)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
        assert!(matches!(
            render_rx.recv_timeout(std::time::Duration::from_secs(1)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn semantic_text_press_does_not_create_a_server_release_lease() {
        let mut client = shell_client();
        client.track_shell_input(
            &shepr_protocol::PublicPaneId::new(&crate::test_support::test_workspace_id("w1"), 1),
            &[ClientPaneInputEvent::Key {
                code: shepr_protocol::ClientKeyCode::Char('x'),
                modifiers: shepr_protocol::WireModifiers::NONE,
                kind: ClientKeyKind::Press,
                repeat_count: 1,
                shifted_codepoint: None,
                generated_text: Some("x".into()),
            }],
        );

        assert!(client.drain_shell_held_inputs().is_empty());
    }

    #[test]
    fn held_key_presses_are_released_once_per_key_code() {
        let mut client = shell_client();
        let key = |code, kind| ClientPaneInputEvent::Key {
            code,
            modifiers: shepr_protocol::WireModifiers::SHIFT,
            kind,
            repeat_count: 1,
            shifted_codepoint: None,
            generated_text: None,
        };
        client.track_shell_input(
            &shepr_protocol::PublicPaneId::new(&crate::test_support::test_workspace_id("w1"), 1),
            &[
                key(shepr_protocol::ClientKeyCode::Enter, ClientKeyKind::Press),
                key(shepr_protocol::ClientKeyCode::Enter, ClientKeyKind::Press),
                key(shepr_protocol::ClientKeyCode::Esc, ClientKeyKind::Press),
                key(shepr_protocol::ClientKeyCode::Esc, ClientKeyKind::Release),
            ],
        );

        let held = client.drain_shell_held_inputs();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].target, "w1:p1");
        assert_eq!(
            held[0].release,
            key(shepr_protocol::ClientKeyCode::Enter, ClientKeyKind::Release)
        );
    }

    fn workspace_ids(numbers: &[usize]) -> Vec<WorkspaceId> {
        numbers
            .iter()
            .map(|&number| WorkspaceId::from_number(number).expect("nonzero number"))
            .collect()
    }

    fn topology(ids: &[WorkspaceId], bookmark: Option<usize>) -> ClientShellTopology {
        ClientShellTopology {
            workspace_ids: ids.to_vec(),
            bookmark_index: bookmark,
        }
    }

    #[test]
    fn a_location_moves_its_generation_only_when_the_viewed_workspace_changes() {
        let ids = workspace_ids(&[1, 2, 3]);
        let mut location = ClientShellLocation::initial(Some((ids[0].clone(), 0)));
        assert_eq!(location.generation(), 1, "initialising is a change");

        // The same workspace at a new index (a reorder) refreshes the index
        // and leaves the projection valid.
        assert!(!location.navigate(ids[0].clone(), 2));
        assert_eq!(location.index(), 2);
        assert_eq!(location.generation(), 1);

        assert!(location.navigate(ids[1].clone(), 1));
        assert_eq!(location.generation(), 2);
    }

    #[test]
    fn a_vanished_workspace_lands_on_the_one_now_at_its_remembered_index() {
        let ids = workspace_ids(&[1, 2, 3, 4]);
        let mut location = ClientShellLocation::initial(Some((ids[1].clone(), 1)));

        // The workspace at index 1 closed: the next one slides into its slot.
        let after_close = [ids[0].clone(), ids[2].clone(), ids[3].clone()];
        assert!(location.reconcile(&topology(&after_close, Some(0))));
        assert_eq!(location.focused_workspace_id.as_ref(), Some(&ids[2]));
        assert_eq!(location.index(), 1);

        // A survivor keeps its workspace when others move around it.
        let reordered = [ids[3].clone(), ids[2].clone(), ids[0].clone()];
        assert!(!location.reconcile(&topology(&reordered, Some(0))));
        assert_eq!(location.focused_workspace_id.as_ref(), Some(&ids[2]));
        assert_eq!(location.index(), 1);

        // Past the end it clamps to the last workspace.
        let mut at_end = ClientShellLocation::initial(Some((ids[3].clone(), 3)));
        let shrunk = [ids[0].clone(), ids[1].clone()];
        assert!(at_end.reconcile(&topology(&shrunk, None)));
        assert_eq!(at_end.focused_workspace_id.as_ref(), Some(&ids[1]));

        // With nothing left it views nothing.
        assert!(at_end.reconcile(&topology(&[], None)));
        assert_eq!(at_end.focused_workspace_id, None);
    }

    #[test]
    fn a_client_viewing_nothing_starts_on_the_bookmark_else_the_first_workspace() {
        let ids = workspace_ids(&[1, 2]);

        let mut bookmarked = ClientShellLocation::default();
        assert!(bookmarked.reconcile(&topology(&ids, Some(1))));
        assert_eq!(bookmarked.focused_workspace_id.as_ref(), Some(&ids[1]));

        let mut first = ClientShellLocation::default();
        assert!(first.reconcile(&topology(&ids, None)));
        assert_eq!(first.focused_workspace_id.as_ref(), Some(&ids[0]));

        let mut empty = ClientShellLocation::default();
        assert!(!empty.reconcile(&topology(&[], None)));
        assert_eq!(empty.generation(), 0);
    }
}
