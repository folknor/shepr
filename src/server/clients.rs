use std::collections::HashMap;
use std::ops::Index;

use crate::api::RenderDemand;
use crate::protocol::PublicTabId;
use crate::protocol::TerminalId;
use crate::protocol::{
    ClientKeyCode, ClientKeyKind, ClientMouseButton, ClientMouseKind, ClientPaneInputEvent,
    RenderEncoding,
};
use crate::server::client_transport::ClientWriter;
use crate::server::render_stream::ClientRenderState;

/// Identity of a connection accepted by this server. Only the registry's
/// allocator mints production values; disconnecting never reuses one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ClientId(u64);

/// Monotonic ordering of accepted client activity within one server run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ActivityStamp(u64);

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

impl ClientId {
    #[cfg(test)]
    pub(crate) fn test_new(value: u64) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
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

#[derive(Debug)]
pub(crate) enum ClientConnectionMode {
    ClientShell(Box<ClientShellState>),
    TerminalPending,
    TerminalAttach {
        terminal_id: TerminalId,
        state: TerminalAttachState,
    },
}

#[derive(Debug, Default)]
pub(crate) struct ClientShellState {
    /// Whether this shell currently receives pane surfaces.
    pub(crate) surface_active: bool,
    /// Whether this shell wants host mouse capture without pane demand.
    pub(crate) mouse_capture: bool,
    /// Last host terminal default colors reported by this shell.
    pub(crate) host_terminal_theme: crate::host_term::theme::TerminalTheme,
    /// Last host light/dark appearance reported by this shell.
    pub(crate) host_terminal_appearance: Option<crate::host_term::theme::HostAppearance>,
    /// Whether appearance came from an explicit host color-scheme report.
    pub(crate) host_terminal_appearance_explicit: bool,
    /// Last reported focus state for this shell's outer terminal.
    pub(crate) outer_terminal_focus: Option<bool>,
    /// Last focused-pane report-all demand sent to this shell.
    pub(crate) host_keyboard_report_all_active: Option<bool>,
    /// Presses forwarded by this shell that need release on abrupt teardown.
    held_inputs: HashMap<ClientShellPressId, ClientShellHeldInput>,
    /// Connection-local workspace and tab projection.
    pub(crate) location: Option<ClientShellLocation>,
    /// Last coherent shell replacement sent to this client.
    pub(crate) snapshot: Option<crate::protocol::ClientShellSnapshot>,
    /// Monotonic shell replacement revision for this connection.
    pub(crate) projection_revision: crate::protocol::ProjectionRevision,
    /// Whether this shell is waiting for one ordered endpoint command response.
    pub(crate) endpoint_command_in_flight: bool,
}

impl ClientShellState {
    pub(crate) fn active() -> Self {
        Self {
            surface_active: true,
            ..Self::default()
        }
    }

    fn update_host_theme(&mut self, update: &crate::protocol::ClientHostThemeUpdate) -> bool {
        let mut next_theme = self.host_terminal_theme;
        let mut changed = false;

        match update {
            crate::protocol::ClientHostThemeUpdate::DefaultColor { kind, color } => {
                let kind: crate::host_term::theme::DefaultColorKind = (*kind).into();
                let color = (*color).into();
                next_theme = next_theme.with_color(kind, color);
                if matches!(kind, crate::host_term::theme::DefaultColorKind::Background)
                    && !self.host_terminal_appearance_explicit
                {
                    changed |= self.set_host_appearance(Some(color.inferred_appearance()), false);
                }
            }
            crate::protocol::ClientHostThemeUpdate::PaletteColors(colors) => {
                for &(index, color) in colors {
                    next_theme = next_theme.with_palette_color(index, color.into());
                }
            }
            crate::protocol::ClientHostThemeUpdate::Appearance(appearance) => {
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
        appearance: Option<crate::host_term::theme::HostAppearance>,
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

#[derive(Debug, Default)]
pub(crate) struct TerminalAttachState {
    /// Last keyboard protocol state sent to a directly attached terminal.
    pub(crate) host_keyboard_protocol_active: Option<(u16, u8)>,
    /// Whether a drop notice is owed until the next input reaches the pane.
    pub(crate) input_drop_reported: bool,
}

impl ClientConnectionMode {
    pub(crate) fn shell() -> Self {
        Self::ClientShell(Box::new(ClientShellState::active()))
    }

    pub(crate) fn terminal_attach(terminal_id: TerminalId) -> Self {
        Self::TerminalAttach {
            terminal_id,
            state: TerminalAttachState::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RenderTargetMode {
    Shell,
    TerminalAttach { terminal_id: TerminalId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderTarget {
    pub(crate) client_id: ClientId,
    pub(crate) terminal_size: crate::geometry::GridSize,
    pub(crate) cell_size: crate::host_term::cell_size::HostCellSize,
    pub(crate) is_foreground: bool,
    pub(crate) mode: RenderTargetMode,
}

/// Pure client identity and ownership state for one headless server.
///
/// Connection accessors support the transport and rendering paths that need to
/// inspect a connection, while cross-connection decisions and ownership maps
/// live here and can be tested without a PTY.
pub(crate) struct ClientRegistry {
    connections: HashMap<ClientId, ClientConnection>,
    next_client_id: u64,
    foreground_client_id: Option<ClientId>,
    geometry_controllers: HashMap<PublicTabId, ClientId>,
    attach_owners: HashMap<TerminalId, ClientId>,
    next_activity_stamp: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachClaim {
    Available,
    AlreadyOwned,
    Reject { owner: ClientId },
    Takeover { owner: ClientId },
}

impl Default for ClientRegistry {
    fn default() -> Self {
        Self {
            connections: HashMap::new(),
            next_client_id: 1,
            foreground_client_id: None,
            geometry_controllers: HashMap::new(),
            attach_owners: HashMap::new(),
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

    #[cfg(test)]
    pub(crate) fn contains_key<K: Copy + Into<ClientId>>(&self, client_id: &K) -> bool {
        self.connections.contains_key(&(*client_id).into())
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

    pub(crate) fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    pub(crate) fn allocate_client_id(&mut self) -> ClientId {
        let id = self.next_client_id;
        self.next_client_id = self.next_client_id.saturating_add(1);
        ClientId(id)
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

    pub(crate) fn set_foreground_client_id(&mut self, client_id: Option<ClientId>) {
        self.foreground_client_id = client_id;
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
            .filter(|client| client.is_active_shell_client() && client.writer.is_some())
            .count()
    }

    pub(crate) fn remove_client(
        &mut self,
        client_id: ClientId,
    ) -> (Option<ClientConnection>, bool) {
        let was_foreground = self.foreground_client_id == Some(client_id);
        let removed = self.connections.remove(&client_id);
        self.remove_geometry_controllers_for(client_id);
        if let Some(ClientConnectionMode::TerminalAttach { terminal_id, .. }) =
            removed.as_ref().map(|client| &client.mode)
            && self.attach_owners.get(terminal_id) == Some(&client_id)
        {
            self.attach_owners.remove(terminal_id);
        }
        if was_foreground {
            self.foreground_client_id = None;
        }
        (removed, was_foreground)
    }

    pub(crate) fn clear(&mut self) {
        self.connections.clear();
        self.foreground_client_id = None;
        self.geometry_controllers.clear();
        self.attach_owners.clear();
    }

    pub(crate) fn geometry_controllers(&self) -> &HashMap<PublicTabId, ClientId> {
        &self.geometry_controllers
    }

    pub(crate) fn geometry_controller(&self, tab_id: &str) -> Option<ClientId> {
        self.geometry_controllers
            .get(&tab_id.parse::<PublicTabId>().ok()?)
            .copied()
    }

    pub(crate) fn geometry_controller_by_id(&self, tab_id: &PublicTabId) -> Option<ClientId> {
        self.geometry_controllers.get(tab_id).copied()
    }

    pub(crate) fn set_geometry_controller(
        &mut self,
        tab_id: &str,
        client_id: ClientId,
    ) -> Option<ClientId> {
        self.geometry_controllers
            .insert(tab_id.parse().ok()?, client_id)
    }

    pub(crate) fn claim_geometry(&mut self, tab_id: &str, client_id: ClientId) -> bool {
        let Ok(tab_id) = tab_id.parse::<PublicTabId>() else {
            return false;
        };
        if !self
            .connections
            .get(&client_id)
            .is_some_and(ClientConnection::is_active_shell_client)
        {
            return false;
        }
        self.geometry_controllers.insert(tab_id, client_id) != Some(client_id)
    }

    pub(crate) fn claim_unowned_geometry(&mut self, tab_id: &str, client_id: ClientId) -> bool {
        let Ok(tab_id) = tab_id.parse::<PublicTabId>() else {
            return false;
        };
        if !self
            .connections
            .get(&client_id)
            .is_some_and(ClientConnection::is_active_shell_client)
        {
            return false;
        }
        if self.geometry_controllers.contains_key(&tab_id) {
            return false;
        }
        self.geometry_controllers.insert(tab_id, client_id);
        true
    }

    pub(crate) fn retain_geometry_controllers(
        &mut self,
        mut keep: impl FnMut(&PublicTabId, ClientId) -> bool,
    ) {
        self.geometry_controllers
            .retain(|tab_id, client_id| keep(tab_id, *client_id));
    }

    pub(crate) fn remove_geometry_controllers_for(&mut self, client_id: ClientId) {
        self.geometry_controllers
            .retain(|_, controller_id| *controller_id != client_id);
    }

    pub(crate) fn attach_owner(&self, terminal_id: &TerminalId) -> Option<ClientId> {
        self.attach_owners.get(terminal_id).copied()
    }

    pub(crate) fn attach_claim(
        &self,
        terminal_id: &TerminalId,
        client_id: ClientId,
        takeover: bool,
    ) -> AttachClaim {
        match self.attach_owner(terminal_id) {
            None => AttachClaim::Available,
            Some(owner) if owner == client_id => AttachClaim::AlreadyOwned,
            Some(owner) if takeover => AttachClaim::Takeover { owner },
            Some(owner) => AttachClaim::Reject { owner },
        }
    }

    pub(crate) fn set_attach_owner(&mut self, terminal_id: TerminalId, client_id: ClientId) {
        self.attach_owners.insert(terminal_id, client_id);
    }

    pub(crate) fn has_attach_owner(&self, terminal_id: &TerminalId) -> bool {
        self.attach_owners.contains_key(terminal_id)
    }

    #[cfg(test)]
    pub(crate) fn attach_owners(&self) -> &HashMap<TerminalId, ClientId> {
        &self.attach_owners
    }
}

/// A held press, keyed by what the client reports: the key code (a Linux
/// terminal reports no physical key identity) or the mouse button.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ClientShellPressId {
    Key(ClientKeyCode),
    Mouse(ClientMouseButton),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClientShellHeldInput {
    pub(crate) target: crate::protocol::PublicPaneId,
    pub(crate) release: ClientPaneInputEvent,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClientShellLocation {
    pub(crate) focused_workspace_id: Option<crate::protocol::WorkspaceId>,
    pub(crate) active_tab_ids: HashMap<crate::protocol::WorkspaceId, PublicTabId>,
}

pub(crate) struct ClientShellTopology {
    pub(crate) focused_workspace_id: Option<crate::protocol::WorkspaceId>,
    pub(crate) fallback_workspace_id: Option<crate::protocol::WorkspaceId>,
    pub(crate) active_tab_ids: HashMap<crate::protocol::WorkspaceId, PublicTabId>,
    pub(crate) tab_workspace_ids: HashMap<PublicTabId, crate::protocol::WorkspaceId>,
}

impl ClientShellLocation {
    pub(crate) fn from_snapshot(snapshot: &crate::protocol::ClientShellSnapshot) -> Self {
        Self {
            focused_workspace_id: snapshot.focused_workspace_id.clone(),
            active_tab_ids: snapshot
                .workspaces
                .iter()
                .map(|workspace| {
                    (
                        workspace.workspace_id.clone(),
                        workspace.active_tab_id.clone(),
                    )
                })
                .collect(),
        }
    }

    pub(crate) fn focused_tab_id(&self) -> Option<&PublicTabId> {
        self.focused_workspace_id
            .as_ref()
            .and_then(|workspace_id| self.active_tab_ids.get(workspace_id))
    }

    pub(crate) fn focus_workspace(&mut self, workspace_id: crate::protocol::WorkspaceId) {
        self.focused_workspace_id = Some(workspace_id);
    }

    pub(crate) fn focus_tab(
        &mut self,
        workspace_id: crate::protocol::WorkspaceId,
        tab_id: PublicTabId,
    ) {
        self.focused_workspace_id = Some(workspace_id.clone());
        self.active_tab_ids.insert(workspace_id, tab_id);
    }

    pub(crate) fn reconcile(&mut self, topology: &ClientShellTopology) {
        self.active_tab_ids.retain(|workspace_id, tab_id| {
            topology.active_tab_ids.contains_key(workspace_id)
                && topology.tab_workspace_ids.get(tab_id) == Some(workspace_id)
        });
        for (workspace_id, tab_id) in &topology.active_tab_ids {
            self.active_tab_ids
                .entry(workspace_id.clone())
                .or_insert_with(|| tab_id.clone());
        }
        if self
            .focused_workspace_id
            .as_ref()
            .is_none_or(|workspace_id| !topology.active_tab_ids.contains_key(workspace_id))
        {
            self.focused_workspace_id = topology
                .focused_workspace_id
                .clone()
                .or_else(|| topology.fallback_workspace_id.clone());
        }
    }
}

/// A connected client tracked by the server.
pub(crate) struct ClientConnection {
    /// State carried by this connection's current client mode.
    pub(crate) mode: ClientConnectionMode,
    /// The client's terminal size after clamping.
    pub(crate) terminal_size: crate::geometry::GridSize,
    /// Pixel size of one client terminal cell.
    pub(crate) cell_size: crate::host_term::cell_size::HostCellSize,
    /// Monotonic activity stamp used to choose the fallback foreground client.
    pub(crate) last_activity: ActivityStamp,
    /// Render baseline for the negotiated client encoding.
    pub(crate) render_state: ClientRenderState,
    /// Whether this frontend preserves exact SGR pixel reports.
    pub(crate) pixel_mouse: bool,
    /// Whether an ordinary render was skipped because the render channel was full.
    pub(crate) render_pending: RenderDemand,
    /// Whether the client has been told that its current frame is too large to
    /// send. Set on the first oversized frame, cleared once a frame goes out, so
    /// a client whose frames keep failing is warned once rather than per render.
    pub(crate) oversized_frame_reported: bool,
    /// Last host mouse capture mode sent to this client.
    pub(crate) host_mouse_capture_active: Option<bool>,
    /// Last SGR pixel provenance mode sent to this client.
    pub(crate) host_sgr_pixels_active: Option<bool>,
    /// Channels for sending framed ServerMessage data to the client writer thread.
    ///
    /// Always `Some` in production: every accepted connection brings a writer,
    /// and a detach removes the client outright rather than keeping a
    /// writer-less entry. `None` exists for test fixtures that exercise
    /// server state without a transport; the `writer.is_none()` checks in the
    /// server serve those. Making the field non-optional would mean giving
    /// every such fixture a channel pair.
    pub(crate) writer: Option<ClientWriter>,
}

impl ClientConnection {
    #[cfg(test)]
    pub(crate) fn new(
        terminal_size: (u16, u16),
        cell_size: crate::host_term::cell_size::HostCellSize,
        last_activity: impl Into<ActivityStamp>,
        render_encoding: RenderEncoding,
        writer: Option<ClientWriter>,
    ) -> Self {
        Self::new_with_mode(
            ClientConnectionMode::shell(),
            crate::geometry::GridSize::clamped(terminal_size.0, terminal_size.1),
            cell_size,
            last_activity,
            render_encoding,
            writer,
        )
    }

    pub(crate) fn new_with_mode(
        mode: ClientConnectionMode,
        terminal_size: crate::geometry::GridSize,
        cell_size: crate::host_term::cell_size::HostCellSize,
        last_activity: impl Into<ActivityStamp>,
        render_encoding: RenderEncoding,
        writer: Option<ClientWriter>,
    ) -> Self {
        Self {
            mode,
            terminal_size,
            cell_size,
            last_activity: last_activity.into(),
            render_state: ClientRenderState::new(render_encoding),
            pixel_mouse: false,
            render_pending: RenderDemand::None,
            oversized_frame_reported: false,
            host_mouse_capture_active: None,
            host_sgr_pixels_active: None,
            writer,
        }
    }

    pub(crate) fn shell_state(&self) -> Option<&ClientShellState> {
        match &self.mode {
            ClientConnectionMode::ClientShell(state) => Some(state),
            ClientConnectionMode::TerminalPending | ClientConnectionMode::TerminalAttach { .. } => {
                None
            }
        }
    }

    pub(crate) fn shell_state_mut(&mut self) -> Option<&mut ClientShellState> {
        match &mut self.mode {
            ClientConnectionMode::ClientShell(state) => Some(state),
            ClientConnectionMode::TerminalPending | ClientConnectionMode::TerminalAttach { .. } => {
                None
            }
        }
    }

    pub(crate) fn terminal_attach_state(&self) -> Option<&TerminalAttachState> {
        match &self.mode {
            ClientConnectionMode::TerminalAttach { state, .. } => Some(state),
            ClientConnectionMode::ClientShell(_) | ClientConnectionMode::TerminalPending => None,
        }
    }

    pub(crate) fn terminal_attach_state_mut(&mut self) -> Option<&mut TerminalAttachState> {
        match &mut self.mode {
            ClientConnectionMode::TerminalAttach { state, .. } => Some(state),
            ClientConnectionMode::ClientShell(_) | ClientConnectionMode::TerminalPending => None,
        }
    }

    pub(crate) fn attach_to_terminal(&mut self, terminal_id: TerminalId) -> bool {
        if !matches!(self.mode, ClientConnectionMode::TerminalPending) {
            return false;
        }
        self.mode = ClientConnectionMode::terminal_attach(terminal_id);
        true
    }

    pub(crate) fn request_repaint(&mut self) {
        self.render_state.request_repaint();
    }

    pub(crate) fn request_recompute(&mut self) {
        self.render_state.request_recompute();
    }

    pub(crate) fn track_shell_input(
        &mut self,
        target: &crate::protocol::PublicPaneId,
        events: &[ClientPaneInputEvent],
    ) {
        let Some(shell) = self.shell_state_mut() else {
            return;
        };
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
                        ClientShellPressId::Key(code.clone()),
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
                        .remove(&ClientShellPressId::Key(code.clone()));
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
                    let id = ClientShellPressId::Mouse(*button);
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
                        .remove(&ClientShellPressId::Mouse(*button));
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
        let Some(shell) = self.shell_state_mut() else {
            return Vec::new();
        };
        shell.held_inputs.drain().map(|(_, held)| held).collect()
    }

    pub(crate) fn update_host_theme(
        &mut self,
        update: &crate::protocol::ClientHostThemeUpdate,
    ) -> bool {
        self.shell_state_mut()
            .is_some_and(|shell| shell.update_host_theme(update))
    }

    pub(crate) fn deferred_render(&self) -> RenderDemand {
        self.render_pending
    }

    pub(crate) fn clear_deferred_render(&mut self) {
        self.render_pending = RenderDemand::None;
    }

    pub(crate) fn defer_full_render(&mut self) {
        self.render_pending.join(RenderDemand::Full);
    }

    pub(crate) fn take_deferred_render(&mut self) -> RenderDemand {
        let deferred = self.deferred_render();
        self.clear_deferred_render();
        deferred
    }

    pub(crate) fn is_shell_client(&self) -> bool {
        matches!(self.mode, ClientConnectionMode::ClientShell(_))
    }

    pub(crate) fn is_active_shell_client(&self) -> bool {
        self.shell_state().is_some_and(|state| state.surface_active)
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

pub(crate) fn terminal_stream_client_ids(
    clients: &ClientRegistry,
    terminal_id: &TerminalId,
) -> Vec<ClientId> {
    clients
        .iter()
        .filter_map(|(&client_id, client)| match &client.mode {
            ClientConnectionMode::TerminalAttach {
                terminal_id: attached,
                ..
            } if attached == terminal_id => Some(client_id),
            _ => None,
        })
        .collect()
}

pub(crate) fn render_targets(
    clients: &ClientRegistry,
    foreground_client_id: Option<ClientId>,
) -> Vec<RenderTarget> {
    let mut targets: Vec<RenderTarget> = clients
        .iter()
        .filter(|(_, client)| {
            client.writer.is_some()
                && (client.is_shell_client()
                    || matches!(client.mode, ClientConnectionMode::TerminalAttach { .. }))
        })
        .filter_map(|(&client_id, client)| {
            let mode = match &client.mode {
                ClientConnectionMode::ClientShell(_) => RenderTargetMode::Shell,
                ClientConnectionMode::TerminalAttach { terminal_id, .. } => {
                    RenderTargetMode::TerminalAttach {
                        terminal_id: terminal_id.clone(),
                    }
                }
                ClientConnectionMode::TerminalPending => return None,
            };
            Some(RenderTarget {
                client_id,
                terminal_size: client.terminal_size,
                cell_size: client.cell_size,
                is_foreground: foreground_client_id == Some(client_id),
                mode,
            })
        })
        .collect();

    targets.sort_by_key(|target| (target.is_foreground, target.client_id));
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell_client() -> ClientConnection {
        ClientConnection::new(
            (80, 24),
            crate::host_term::cell_size::HostCellSize::default(),
            1,
            crate::protocol::RenderEncoding::SemanticFrame,
            None,
        )
    }

    #[test]
    fn only_shell_connections_start_with_an_active_surface() {
        let connection = |mode| {
            ClientConnection::new_with_mode(
                mode,
                crate::geometry::GridSize::clamped(80, 24),
                crate::host_term::cell_size::HostCellSize::default(),
                1,
                crate::protocol::RenderEncoding::TerminalAnsi,
                None,
            )
        };
        assert!(connection(ClientConnectionMode::shell()).is_active_shell_client());
        let pending = connection(ClientConnectionMode::TerminalPending);
        assert!(!pending.is_active_shell_client());
        let attached = connection(ClientConnectionMode::terminal_attach(TerminalId::test_new(
            "t1",
        )));
        assert!(!attached.is_active_shell_client());

        let mut clients = HashMap::new();
        clients.insert(ClientId::test_new(1), pending);
        clients.insert(ClientId::test_new(2), attached);
        assert_eq!(latest_shell_client(&clients), None);
    }

    #[test]
    fn registry_owns_foreground_and_attach_arbitration() {
        let mut registry = ClientRegistry::default();
        let first_id = registry.allocate_client_id();
        let second_id = registry.allocate_client_id();
        assert_eq!(
            (first_id, second_id),
            (ClientId::test_new(1), ClientId::test_new(2))
        );
        let first = ClientConnection::new(
            (80, 24),
            crate::host_term::cell_size::HostCellSize::default(),
            registry.allocate_activity_stamp(),
            crate::protocol::RenderEncoding::SemanticFrame,
            None,
        );
        let second = ClientConnection::new_with_mode(
            ClientConnectionMode::TerminalPending,
            crate::geometry::GridSize::clamped(80, 24),
            crate::host_term::cell_size::HostCellSize::default(),
            registry.allocate_activity_stamp(),
            crate::protocol::RenderEncoding::TerminalAnsi,
            None,
        );
        registry.insert(first_id, first);
        registry.insert(second_id, second);

        assert!(registry.promote_to_foreground(first_id));
        assert_eq!(registry.foreground_client_id(), Some(first_id));
        assert!(!registry.promote_to_foreground(second_id));
        assert!(registry.claim_geometry("w1:t1", first_id));
        assert!(!registry.claim_unowned_geometry("w1:t1", first_id));
        assert_eq!(registry.geometry_controller("w1:t1"), Some(first_id));

        let terminal_id = TerminalId::test_new("terminal-a");
        registry.set_attach_owner(terminal_id.clone(), second_id);
        assert_eq!(registry.attach_owner(&terminal_id), Some(second_id));
        assert_eq!(
            registry.attach_claim(&terminal_id, first_id, false),
            AttachClaim::Reject { owner: second_id }
        );
        assert_eq!(
            registry.attach_claim(&terminal_id, first_id, true),
            AttachClaim::Takeover { owner: second_id }
        );
        assert!(
            registry
                .get_mut(&second_id)
                .is_some_and(|client| client.attach_to_terminal(terminal_id.clone()))
        );
        let (_, was_foreground) = registry.remove_client(first_id);
        assert!(was_foreground);
        assert_eq!(registry.geometry_controller("w1:t1"), None);
        assert!(!registry.promote_latest_remaining());
        assert_eq!(registry.foreground_client_id(), None);
        let (removed, was_foreground) = registry.remove_client(second_id);
        assert!(removed.is_some());
        assert!(!was_foreground);
        assert_eq!(registry.attach_owner(&terminal_id), None);
    }

    #[test]
    fn semantic_text_press_does_not_create_a_server_release_lease() {
        let mut client = shell_client();
        client.track_shell_input(
            &"w1:p1".into(),
            &[ClientPaneInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('x'),
                modifiers: crate::protocol::WireModifiers::NONE,
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
            modifiers: crate::protocol::WireModifiers::SHIFT,
            kind,
            repeat_count: 1,
            shifted_codepoint: None,
            generated_text: None,
        };
        client.track_shell_input(
            &"w1:p1".into(),
            &[
                key(crate::protocol::ClientKeyCode::Enter, ClientKeyKind::Press),
                key(crate::protocol::ClientKeyCode::Enter, ClientKeyKind::Press),
                key(crate::protocol::ClientKeyCode::Esc, ClientKeyKind::Press),
                key(crate::protocol::ClientKeyCode::Esc, ClientKeyKind::Release),
            ],
        );

        let held = client.drain_shell_held_inputs();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].target, "w1:p1");
        assert_eq!(
            held[0].release,
            key(
                crate::protocol::ClientKeyCode::Enter,
                ClientKeyKind::Release
            )
        );
    }
}
